use core::cmp::Ordering;
use core::mem::MaybeUninit;
use core::slice;

use heapless::Vec;

use crate::config::*;
use crate::file::{FileManager, FileReader, FileSearcher, FileWriter, SeekDirection, Seq};
use crate::flash::Flash;
use crate::page::ReadError;
use crate::{Error, ReadKeyError};

pub struct Database<F: Flash> {
    files: FileManager<F>,
}

impl<F: Flash> Database<F> {
    pub fn format(flash: F) {
        let mut m = FileManager::new(flash);
        m.format();
    }

    pub fn new(flash: F) -> Result<Self, Error> {
        let mut m = FileManager::new(flash);
        m.mount()?;

        // TODO recover from this
        if !m.is_empty(0) {
            corrupted!();
        }

        Ok(Self { files: m })
    }

    pub fn flash_mut(&mut self) -> &mut F {
        self.files.flash_mut()
    }

    pub fn read_transaction(&mut self) -> Result<ReadTransaction<'_, F>, Error> {
        Ok(ReadTransaction { db: self })
    }

    pub fn write_transaction(&mut self) -> Result<WriteTransaction<'_, F>, Error> {
        trace!("record wtx: start");

        let file_id = loop {
            match self.find_empty_file_in_level(LEVEL_COUNT - 1) {
                Some(f) => break f,
                None => {
                    trace!("record wtx: no free file, compacting.");
                    let did_something = self.compact()?;

                    // if last level is full, compact should always
                    // find something to do.
                    assert!(did_something);
                }
            }
        };

        trace!("record wtx: writing file {}", file_id);
        let w = self.files.write(file_id);
        Ok(WriteTransaction {
            db: self,
            w,
            last_key: Vec::new(),
        })
    }

    fn file_id(level: usize, index: usize) -> FileID {
        (1 + level * BRANCHING_FACTOR + index) as _
    }

    /// Returns None if level is full.
    fn find_empty_file_in_level(&mut self, level: usize) -> Option<FileID> {
        for i in 0..BRANCHING_FACTOR {
            let file_id = Self::file_id(level, i);
            if self.files.is_empty(file_id) {
                return Some(file_id);
            }
        }
        None
    }

    fn is_level_full(&self, level: usize) -> bool {
        (0..BRANCHING_FACTOR).all(|i| !self.files.is_empty(Self::file_id(level, i)))
    }

    fn is_level_empty(&self, level: usize) -> bool {
        (0..BRANCHING_FACTOR).all(|i| self.files.is_empty(Self::file_id(level, i)))
    }

    fn level_file_count(&self, level: usize) -> usize {
        (0..BRANCHING_FACTOR)
            .filter(|&i| !self.files.is_empty(Self::file_id(level, i)))
            .count()
    }

    fn compact_find_work(&mut self) -> Result<Option<(Vec<FileID, BRANCHING_FACTOR>, FileID)>, Error> {
        const FILE_FLAG_COMPACT_DEST: u8 = 0x01;
        const FILE_FLAG_COMPACT_SRC: u8 = 0x02;

        // Check if there's an in-progress compaction that we should continue.
        match self.files.files_with_flag(FILE_FLAG_COMPACT_DEST).single() {
            Ok(dst) => {
                let mut src = Vec::new();
                for src_file in self.files.files_with_flag(FILE_FLAG_COMPACT_SRC) {
                    if src_file <= dst {
                        // All src files should be after dst in the tree.
                        corrupted!()
                    }

                    if let Err(_) = src.push(src_file) {
                        // at most BRANCHING_FACTOR src files
                        corrupted!()
                    }
                }
                return Ok(Some((src, dst)));
            }
            Err(SingleError::MultipleElements) => corrupted!(), // should never happen
            Err(SingleError::NoElements) => {}                  // no compaction in progress
        }

        // File 0 should always be empty if there's no in-progress compaction.
        if !self.files.is_empty(0) {
            corrupted!()
        }

        // Otherwise, start a new compaction.

        // Find a level...
        let lv = (0..LEVEL_COUNT)
            // ... that we can compact (level above is not full)
            .filter(|&lv| lv == 0 || !self.is_level_full(lv - 1))
            // ... and that is the fullest.
            // In case of a tie, pick the lowest level (max_by_key picks the latest element on ties)
            .max_by_key(|&lv| self.level_file_count(lv));

        let Some(lv) = lv else {
            // No compaction work to do.
            return Ok(None)
        };

        // destination file
        let dst = if lv == 0 {
            0
        } else {
            self.find_empty_file_in_level(lv - 1).unwrap()
        };

        // source files
        let mut src = Vec::new();
        for i in 0..BRANCHING_FACTOR {
            let src_file = Self::file_id(lv, i);
            if !self.files.is_empty(src_file) {
                src.push(src_file).unwrap();
            }
        }

        if src.is_empty() || (src.len() == 1 && lv == 0) {
            // No compaction work to do.
            return Ok(None);
        }

        Ok(Some((src, dst)))
    }

    fn do_compact(&mut self, src: Vec<FileID, BRANCHING_FACTOR>, dst: FileID) -> Result<(), Error> {
        trace!("record: compacting {:?} -> {}", src, dst);

        let mut w = self.files.write(dst);

        // Open all files in level for reading.
        let mut r: [MaybeUninit<FileReader>; BRANCHING_FACTOR] = unsafe { MaybeUninit::uninit().assume_init() };
        for (i, &file_id) in src.iter().enumerate() {
            r[i].write(self.files.read(file_id));
        }
        let r = unsafe { slice::from_raw_parts_mut(r.as_mut_ptr() as *mut FileReader, src.len()) };

        let m = &mut self.files;

        fn read_key_or_empty<F: Flash>(
            m: &mut FileManager<F>,
            r: &mut FileReader,
            buf: &mut Vec<u8, MAX_KEY_SIZE>,
        ) -> Result<(), Error> {
            match read_key(m, r, buf) {
                Ok(()) => Ok(()),
                Err(ReadError::Eof) => Ok(buf.truncate(0)),
                Err(ReadError::Corrupted) => corrupted!(),
            }
        }

        const NEW_VEC: Vec<u8, MAX_KEY_SIZE> = Vec::new();
        let mut k = [NEW_VEC; BRANCHING_FACTOR];

        for i in 0..src.len() {
            read_key_or_empty(m, &mut r[i], &mut k[i])?;
        }

        loop {
            fn highest_bit(x: u32) -> Option<usize> {
                match x {
                    0 => None,
                    _ => Some(31 - x.leading_zeros() as usize),
                }
            }

            let mut bits: u32 = 0;
            for i in 0..src.len() {
                // Ignore files that have already reached the end.
                if k[i].is_empty() {
                    continue;
                }

                match highest_bit(bits) {
                    // If we haven't found any nonempty key yet, take the current one.
                    None => bits = 1 << i,
                    Some(j) => match k[j].cmp(&k[i]) {
                        Ordering::Greater => bits = 1 << i,
                        Ordering::Equal => bits |= 1 << i,
                        Ordering::Less => {}
                    },
                }
            }

            // All keys empty, means we've finished
            if bits == 0 {
                break;
            }

            match highest_bit(bits) {
                // All keys empty, means we've finished
                None => break,
                Some(i) => {
                    // Copy value from the highest bit (so newest file)
                    write_key(m, &mut w, &k[i])?;
                    copy_value(m, &mut r[i], &mut w)?;
                    read_key_or_empty(m, &mut r[i], &mut k[i])?;

                    // Advance all the others
                    for j in 0..BRANCHING_FACTOR {
                        if j != i && (bits & 1 << j) != 0 {
                            check_corrupted!(skip_value(m, &mut r[j]));
                            read_key_or_empty(m, &mut r[j], &mut k[j])?;
                        }
                    }
                }
            }
        }

        let mut truncate = [(0, usize::MAX); BRANCHING_FACTOR];
        for (i, &file_id) in src.iter().enumerate() {
            truncate[i] = (file_id, usize::MAX);
        }
        self.files.commit_and_truncate(Some(&mut w), &truncate[..src.len()])?;

        // special case: if compacting from level 0
        if dst == 0 {
            self.files.rename(0, Self::file_id(0, 0))?;
        }

        Ok(())
    }

    fn compact(&mut self) -> Result<bool, Error> {
        let Some((src, dst)) = self.compact_find_work()? else{
            return Ok(false)
        };

        self.do_compact(src, dst)?;
        Ok(true)
    }

    #[cfg(feature = "std")]
    pub fn dump(&mut self) {
        for file_id in 0..FILE_COUNT {
            debug!("====== FILE {} ======", file_id);
            if let Err(e) = self.dump_file(file_id as _) {
                debug!("failed to dump file: {:?}", e);
            }
        }
    }

    #[cfg(feature = "std")]
    pub fn dump_file(&mut self, file_id: FileID) -> Result<(), Error> {
        self.files.dump_file(file_id)?;

        let mut r = self.files.read(file_id);
        let mut key = Vec::new();
        let mut value = [0u8; 1024];
        loop {
            let seq = r.curr_seq(&mut self.files);
            match read_key(&mut self.files, &mut r, &mut key) {
                Ok(()) => {}
                Err(ReadError::Eof) => break,
                Err(ReadError::Corrupted) => corrupted!(),
            }
            let n = check_corrupted!(read_value(&mut self.files, &mut r, &mut value));
            let value = &value[..n];

            debug!("record at seq={:?}: key={:02x?} value={:02x?}", seq, key, value);
        }

        Ok(())
    }
}

pub struct ReadTransaction<'a, F: Flash + 'a> {
    db: &'a mut Database<F>,
}

impl<'a, F: Flash + 'a> ReadTransaction<'a, F> {
    pub fn read(&mut self, key: &[u8], value: &mut [u8]) -> Result<usize, ReadKeyError> {
        for file_id in (0..FILE_COUNT).rev() {
            if let Some(res) = self.read_in_file(file_id as _, key, value)? {
                return Ok(res);
            }
        }
        Ok(0)
    }

    fn read_in_file(&mut self, file_id: FileID, key: &[u8], value: &mut [u8]) -> Result<Option<usize>, ReadKeyError> {
        let r = self.db.files.read(file_id);
        let m = &mut self.db.files;
        let mut s = FileSearcher::new(r);

        let mut key_buf = Vec::new();

        // Binary search
        let mut ok = s.start(m)?;
        while ok {
            match read_key(m, s.reader(), &mut key_buf) {
                Err(ReadError::Eof) => return Ok(None), // key not present.
                x => x?,
            };

            // Found?
            let dir = match key_buf[..].cmp(key) {
                Ordering::Equal => return Ok(Some(read_value(m, s.reader(), value)?)),
                Ordering::Less => SeekDirection::Right,
                Ordering::Greater => SeekDirection::Left,
            };

            // Not found, do a binary search step.
            ok = s.seek(m, dir)?;
        }

        let r = s.reader();

        // Linear search
        loop {
            match read_key(m, r, &mut key_buf) {
                Err(ReadError::Eof) => return Ok(None), // key not present.
                x => x?,
            }

            // Found?
            match key_buf[..].cmp(key) {
                Ordering::Equal => return Ok(Some(read_value(m, r, value)?)),
                Ordering::Less => {}                  // keep going
                Ordering::Greater => return Ok(None), // not present.
            }

            skip_value(m, r)?;
        }
    }
}

pub struct WriteTransaction<'a, F: Flash + 'a> {
    db: &'a mut Database<F>,
    w: FileWriter,
    last_key: Vec<u8, MAX_KEY_SIZE>,
}

impl<'a, F: Flash + 'a> WriteTransaction<'a, F> {
    pub fn write(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if key.is_empty() {
            panic!("key cannot be empty.")
        }
        if key.len() > MAX_KEY_SIZE {
            panic!("key too long.")
        }

        if key <= &self.last_key {
            panic!("writes within a transaction must be sorted.");
        }
        self.last_key = Vec::from_slice(key).unwrap();

        write_record(&mut self.db.files, &mut self.w, key, value)?;

        Ok(())
    }

    pub fn commit(mut self) -> Result<(), Error> {
        self.db.files.commit(&mut self.w)
    }
}

fn write_record<F: Flash>(m: &mut FileManager<F>, w: &mut FileWriter, key: &[u8], value: &[u8]) -> Result<(), Error> {
    write_key(m, w, key)?;
    write_value(m, w, value)?;
    Ok(())
}

fn write_key<F: Flash>(m: &mut FileManager<F>, w: &mut FileWriter, key: &[u8]) -> Result<(), Error> {
    let key_len: u32 = key.len().try_into().unwrap();
    write_leb128(m, w, key_len)?;
    w.write(m, key)?;
    Ok(())
}

fn write_value<F: Flash>(m: &mut FileManager<F>, w: &mut FileWriter, value: &[u8]) -> Result<(), Error> {
    let value_len: u32 = value.len().try_into().unwrap();
    write_leb128(m, w, value_len)?;
    w.write(m, value)?;
    w.record_end();
    Ok(())
}

fn copy_value<F: Flash>(m: &mut FileManager<F>, r: &mut FileReader, w: &mut FileWriter) -> Result<(), Error> {
    let mut len = check_corrupted!(read_leb128(m, r)) as usize;
    write_leb128(m, w, len as _)?;

    let mut buf = [0; 128];
    while len != 0 {
        let n = len.min(buf.len());
        len -= n;

        check_corrupted!(r.read(m, &mut buf[..n]));
        w.write(m, &buf[..n])?;
    }
    w.record_end();
    Ok(())
}

fn write_leb128<F: Flash>(m: &mut FileManager<F>, w: &mut FileWriter, mut val: u32) -> Result<(), Error> {
    loop {
        let mut part = val & 0x7F;
        let rest = val >> 7;
        if rest != 0 {
            part |= 0x80
        }

        w.write(m, &[part as u8])?;

        if rest == 0 {
            break;
        }
        val = rest
    }
    Ok(())
}

fn read_key<F: Flash>(
    m: &mut FileManager<F>,
    r: &mut FileReader,
    buf: &mut Vec<u8, MAX_KEY_SIZE>,
) -> Result<(), ReadError> {
    let len = read_leb128(m, r)? as usize;
    if len > MAX_KEY_SIZE {
        info!("key too long: {}", len);
        corrupted!();
    }
    unsafe { buf.set_len(len) };
    r.read(m, buf)
}

fn read_value<F: Flash>(m: &mut FileManager<F>, r: &mut FileReader, value: &mut [u8]) -> Result<usize, ReadKeyError> {
    let len = check_corrupted!(read_leb128(m, r)) as usize;
    if len > value.len() {
        return Err(ReadKeyError::BufferTooSmall);
    }
    r.read(m, &mut value[..len])?;
    Ok(len)
}

fn skip_value<F: Flash>(m: &mut FileManager<F>, r: &mut FileReader) -> Result<(), ReadError> {
    let len = read_leb128(m, r)? as usize;
    r.skip(m, len)?;
    Ok(())
}

fn read_u8<F: Flash>(m: &mut FileManager<F>, r: &mut FileReader) -> Result<u8, ReadError> {
    let mut buf = [0u8; 1];
    r.read(m, &mut buf)?;
    Ok(buf[0])
}

fn read_leb128<F: Flash>(m: &mut FileManager<F>, r: &mut FileReader) -> Result<u32, ReadError> {
    let mut res = 0;
    let mut shift = 0;
    loop {
        let x = read_u8(m, r)?;
        if shift >= 32 {
            corrupted!()
        }
        res |= (x as u32 & 0x7F) << shift;
        if x & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(res)
}

pub trait Single: Iterator {
    /// Get the single element from a single-element iterator.
    fn single(self) -> Result<Self::Item, SingleError>;
}

/// An error in the execution of [`Single::single`](trait.Single.html#tymethod.single).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SingleError {
    /// Asked empty iterator for single element.
    NoElements,
    /// Asked iterator with multiple elements for single element.
    MultipleElements,
}

impl<I: Iterator> Single for I {
    fn single(mut self) -> Result<Self::Item, SingleError> {
        match self.next() {
            None => Err(SingleError::NoElements),
            Some(element) => match self.next() {
                None => Ok(element),
                Some(_) => Err(SingleError::MultipleElements),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use test_log::test;

    use super::*;
    use crate::flash::MemFlash;

    #[test]
    fn test() {
        let mut f = MemFlash::new();
        Database::format(&mut f);

        let mut db = Database::new(&mut f).unwrap();

        let mut buf = [0u8; 1024];

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"bar", b"4321").unwrap();
        wtx.write(b"foo", b"1234").unwrap();
        wtx.commit().unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let n = rtx.read(b"foo", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"1234");
        let n = rtx.read(b"bar", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"4321");
        let n = rtx.read(b"baz", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"");

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"bar", b"8765").unwrap();
        wtx.write(b"baz", b"4242").unwrap();
        wtx.write(b"foo", b"5678").unwrap();
        wtx.commit().unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let n = rtx.read(b"foo", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"5678");
        let n = rtx.read(b"bar", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"8765");
        let n = rtx.read(b"baz", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"4242");

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"lol", b"9999").unwrap();
        wtx.commit().unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let n = rtx.read(b"foo", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"5678");
        let n = rtx.read(b"bar", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"8765");
        let n = rtx.read(b"baz", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"4242");
        let n = rtx.read(b"lol", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"9999");
    }

    #[test]
    fn test_buf_too_small() {
        let mut f = MemFlash::new();
        Database::format(&mut f);

        let mut db = Database::new(&mut f).unwrap();

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"foo", b"1234").unwrap();
        wtx.commit().unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let mut buf = [0u8; 1];
        let r = rtx.read(b"foo", &mut buf);
        assert!(matches!(r, Err(ReadKeyError::BufferTooSmall)));
    }

    #[test]
    fn test_remount() {
        let mut f = MemFlash::new();
        Database::format(&mut f);

        let mut db = Database::new(&mut f).unwrap();

        let mut buf = [0u8; 1024];

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"bar", b"4321").unwrap();
        wtx.write(b"foo", b"1234").unwrap();
        wtx.commit().unwrap();

        // remount
        let mut db = Database::new(&mut f).unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let n = rtx.read(b"foo", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"1234");
        let n = rtx.read(b"bar", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"4321");
        let n = rtx.read(b"baz", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"");

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"bar", b"8765").unwrap();
        wtx.write(b"baz", b"4242").unwrap();
        wtx.write(b"foo", b"5678").unwrap();
        wtx.commit().unwrap();

        // remount
        let mut db = Database::new(&mut f).unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let n = rtx.read(b"foo", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"5678");
        let n = rtx.read(b"bar", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"8765");
        let n = rtx.read(b"baz", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"4242");

        let mut wtx = db.write_transaction().unwrap();
        wtx.write(b"lol", b"9999").unwrap();
        wtx.commit().unwrap();

        // remount
        let mut db = Database::new(&mut f).unwrap();

        let mut rtx = db.read_transaction().unwrap();
        let n = rtx.read(b"foo", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"5678");
        let n = rtx.read(b"bar", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"8765");
        let n = rtx.read(b"baz", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"4242");
        let n = rtx.read(b"lol", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"9999");
    }
}

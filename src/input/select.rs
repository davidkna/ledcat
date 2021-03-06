use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path;
use std::thread;
use std::time;
use nix::{fcntl, poll};


#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WhenEOF {
    Close,
    Retry,
}

pub trait ReadFd: io::Read + AsRawFd { }

impl<T> ReadFd for T
    where T: io::Read + AsRawFd { }

pub struct Reader {
    when_eof: WhenEOF,

    inputs: Vec<Box<ReadFd + Send>>,
    // The number of bytes after which another input is selected.
    switch_after: usize,
    // A buffer for each input to be used for partially received content.
    buffers: Vec<Vec<u8>>,
    // The current buffer selected for output.
    current: io::Cursor<Vec<u8>>,
    // The time after which a partially received frame should be discarded.
    clear_timeout: Option<time::Duration>,
}

impl Reader {
    pub fn from_files<P>(filenames: Vec<P>, switch_after: usize, when_eof: WhenEOF, clear_timeout: Option<time::Duration>) -> io::Result<Reader>
        where P: AsRef<path::Path> {
        let files: io::Result<Vec<Box<ReadFd + Send>>> = filenames.into_iter().map(|filename| {
            let mut open_opts = fs::OpenOptions::new();
            open_opts.read(true);

            let is_fifo = fs::metadata(&filename)?.file_type().is_fifo();
            if is_fifo {
                // A FIFO will block the call to open() until the other end has been opened. This
                // means that when multiple FIFO's are used, they all have to be open at once
                // before this program can continue.
                // Opening the file with O_NONBLOCK will ensure that we don't have to wait.
                // After the file has been opened, there is no need to make reads block again since
                // poll(2) is used to check whether data is available.
                open_opts.custom_flags(fcntl::OFlag::O_NONBLOCK.bits());

                if when_eof == WhenEOF::Retry {
                    // When the first program writing to the FIFO closes the writing end, poll will
                    // immediately return with a POLLHUP for the respective reading end because all
                    // writing ends have been closed. If we open the FIFO for writing ourselves,
                    // there will always be writers. This ensures that poll never returnes POLLHUP.
                    open_opts.write(true);
                }
            }

            let file = open_opts.open(&filename)?;
            Ok(Box::<ReadFd + Send>::from(Box::new(file)))
        }).collect();
        Ok(Reader::from(files?, switch_after, when_eof, clear_timeout))
    }

    pub fn from(inputs: Vec<Box<ReadFd + Send>>, switch_after: usize, when_eof: WhenEOF, clear_timeout: Option<time::Duration>) -> Reader {
        assert_ne!(inputs.len(), 0);
        let buffers = (0..inputs.len())
            .map(|_| Vec::with_capacity(switch_after))
            .collect();
        Reader {
            switch_after,
            buffers,
            when_eof,
            inputs,
            current: io::Cursor::new(Vec::new()),
            clear_timeout,
        }
    }
}

impl io::Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.current.position() == self.current.get_ref().len() as u64 {
            // The end of the current buffer has been reached, fetch more data.
            loop {
                // Perform a poll to see if there are any inputs ready for reading.
                let mut poll_fds: Vec<_> = self.inputs.iter()
                    .map(|inp| {
                        poll::PollFd::new(inp.as_raw_fd(), poll::EventFlags::POLLIN)
                    })
                    .collect();
                let timeout = self.clear_timeout.as_ref()
                    .map(|t| t.as_secs() as i32 * 1_000 + t.subsec_nanos() as i32 / 1_000_000)
                    .unwrap_or(-1);
                if io_err!(poll::poll(&mut poll_fds, timeout))? == 0 {
                    assert!(self.clear_timeout.is_some());
                    // Timeout expired, clear the input buffers.
                    for buf in &mut self.buffers {
                        buf.clear();
                    }
                }

                let mut num_open = poll_fds.len();
                let mut ready_index = None;
                for (i, p) in poll_fds.iter().enumerate() {
                    let rev = p.revents().unwrap();
                    if rev.contains(poll::EventFlags::POLLIN) {
                        let buf = &mut self.buffers[i];
                        let buf_used = buf.len();
                        assert_ne!(buf_used, self.switch_after);
                        // Resize the buffer so there is just enough space for the remainder of the
                        // frame.
                        buf.resize(self.switch_after, 0);

                        let nread = self.inputs[i].read(&mut buf[buf_used..])?;
                        buf.resize(buf_used + nread, 0);
                        assert!(buf.len() <= self.switch_after);
                        if nread == 0 { // EOF
                            num_open -= 1;
                        } else if buf.len() == self.switch_after {
                            ready_index = Some(i);
                            break;
                        }
                    } else if rev.intersects(poll::EventFlags::POLLHUP|poll::EventFlags::POLLNVAL|poll::EventFlags::POLLERR) {
                        num_open -= 1;
                    }
                }

                if num_open == 0 {
                    if self.when_eof == WhenEOF::Close {
                        return Ok(0);
                    }
                    // Prevent a busy wait for inputs that make poll return immediately.
                    let wait = self.clear_timeout
                        .unwrap_or_else(|| time::Duration::new(0, 10_000_000));
                    thread::sleep(wait);
                }

                if let Some(i) = ready_index {
                    let tail = self.buffers[i].split_off(self.switch_after);
                    self.buffers.push(tail); // Later moved to index i by swap_remove.
                    let buf = self.buffers.swap_remove(i);
                    self.current = io::Cursor::new(buf);
                    break;
                }
            }
        }
        self.current.read(buf)
    }
}


#[cfg(test)]
mod tests {
    extern crate rand;
    extern crate tempdir;
    use std::*;
    use std::io::{Seek, Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::sync::mpsc;
    use nix::sys::stat::Mode;
    use nix::unistd;
    use super::*;
    use self::rand::Rng;
    use self::rand::distributions::Alphanumeric;

    macro_rules! timeout {
        ($timeout:expr, $block:block) => {{
            #![cfg_attr(feature="clippy", allow(unit_expr))]
            let (tx, rx) = mpsc::sync_channel(1);
            let thread = thread::spawn(move || {
                let val = $block;
                let _ = tx.send(());
                val
            });
            if rx.recv_timeout($timeout).is_err() {
                panic!("Timeout expired");
            }
            thread.join().unwrap()
        }}
    }

    // FIXME: This function uses memfd, which is not available on Mac OS.
    // FIXME: The CI env can't handle poll(2) or something.
    #[cfg(all(target_os = "linux", not(all(feature = "ci", target_arch = "arm"))))]
    fn new_iter_reader<I>(iter: I) -> Box<fs::File>
        where I: iter::Iterator<Item = u8> {
        use nix::sys::memfd::*;
        let name = rand::thread_rng().sample_iter(&Alphanumeric)
            .take(32)
            .collect::<String>();
        let cname = ffi::CString::new(name).unwrap();
        let fd = memfd_create(&cname, MemFdCreateFlag::empty()).unwrap();
        let mut f = unsafe { fs::File::from_raw_fd(fd) };
        for b in iter {
            f.write_all(&[b]).unwrap();
        }
        f.seek(io::SeekFrom::Start(0)).unwrap();
        Box::new(f)
    }

    fn copy_iter<I: iter::Iterator<Item = u8>>(wr: &mut io::Write, it: I) {
        let v: Vec<u8> = it.collect();
        wr.write_all(&v).unwrap();
        wr.flush().unwrap();
    }

    #[cfg(all(target_os = "linux", not(all(feature = "ci", target_arch = "arm"))))]
    #[test]
    fn read_one_input() {
        let len = 100;
        let num = 16;
        let testdata: Vec<u8> = (1..num + 1)
            .fold(Box::from(iter::empty()) as Box<iter::Iterator<Item = _>>,
                  |ch, i| Box::from(ch.chain(iter::repeat(i as u8).take(len))))
            .collect();

        let mut reader = Reader::from(
            vec![new_iter_reader(testdata.clone().into_iter())],
            len,
            WhenEOF::Close,
            None,
        );

        for i in 0..num {
            let mut rd_buf = vec![0; len];
            reader.read_exact(&mut rd_buf).unwrap();
            assert_eq!(testdata[len * i..len * (i + 1)], rd_buf[..]);
        }
        timeout!(time::Duration::new(10, 0), {
            assert_eq!(0, io::copy(&mut reader, &mut io::sink()).unwrap());
        });
    }

    #[cfg(all(target_os = "linux", not(all(feature = "ci", target_arch = "arm"))))]
    #[test]
    fn read_multiple_inputs_order() {
        let len = 100;
        let num = 16;

        let mut reader = Reader::from(
            (1..num + 1).map(|i| new_iter_reader(iter::repeat(i).take(len)) as Box<ReadFd + Send>).collect(),
            len,
            WhenEOF::Close,
            None,
        );

        for i in 1..num + 1 {
            let mut rd_buf = vec![0; len];
            reader.read_exact(&mut rd_buf).unwrap();
            let expected: Vec<u8> = iter::repeat(i).take(len).collect();
            assert_eq!(expected, rd_buf);
        }
        timeout!(time::Duration::new(10, 0), {
            assert_eq!(0, io::copy(&mut reader, &mut io::sink()).unwrap());
        });
    }

    #[cfg(all(target_os = "linux", not(all(feature = "ci", target_arch = "arm"))))]
    #[test]
    fn read_eof() {
        let mut reader = Reader::from(
            vec![new_iter_reader(iter::empty()), new_iter_reader(iter::empty())],
            1,
            WhenEOF::Close,
            None,
        );
        timeout!(time::Duration::new(10, 0), {
            assert_eq!(0, io::copy(&mut reader, &mut io::sink()).unwrap());
        });
    }

    #[cfg(all(target_os = "linux", not(all(feature = "ci", target_arch = "arm"))))]
    #[test]
    #[should_panic(expected="Timeout expired")]
    fn read_eof_retry() {
        let mut reader = Reader::from(
            vec![new_iter_reader(iter::empty())],
            1,
            WhenEOF::Retry,
            None,
        );
        timeout!(time::Duration::new(0, 100_000_000), {
            io::copy(&mut reader, &mut io::sink()).unwrap();
        });
    }

    #[cfg(not(target_os = "macos"))] // FIXME
    #[test]
    fn read_unix_fifo() {
        let len = 10;
        let (pat1, pat2) = (12, 42);

        let tmp = tempdir::TempDir::new("read_unix_fifo").unwrap();
        let fifo1_path = tmp.path().join("fifo1");
        let fifo2_path = tmp.path().join("fifo2");
        unistd::mkfifo(&fifo1_path, Mode::from_bits(0o666).unwrap()).unwrap();
        unistd::mkfifo(&fifo2_path, Mode::from_bits(0o666).unwrap()).unwrap();

        let mut reader = Reader::from_files(
            vec![&fifo1_path, &fifo2_path],
            len,
            WhenEOF::Close,
            None,
        ).unwrap();
        let mut fifo1 = fs::OpenOptions::new().write(true).open(&fifo1_path).unwrap();
        let mut fifo2 = fs::OpenOptions::new().write(true).open(&fifo2_path).unwrap();

        let mut rd_buf = vec![0; len];

        // Send a partial frame over fifo 1...
        copy_iter(&mut fifo1, iter::repeat(pat1).take(len - 1));

        // Send and receive a full frame over fifo 2.
        let testdata: Vec<u8> = iter::repeat(pat2).take(len).collect();
        copy_iter(&mut fifo2, testdata.clone().into_iter());
        reader.read_exact(&mut rd_buf).unwrap();
        assert_eq!(testdata, rd_buf);
        rd_buf.resize(len, 0);

        // ...and complete that first frame over fifo 1.
        copy_iter(&mut fifo1, iter::once(pat1));
        reader.read_exact(&mut rd_buf).unwrap();
        let expected: Vec<u8> = iter::repeat(pat1).take(len).collect();
        assert_eq!(expected, rd_buf);

        drop(fifo1);
        drop(fifo2);
        timeout!(time::Duration::new(10, 0), {
            assert_eq!(0, io::copy(&mut reader, &mut io::sink()).unwrap());
        });

        tmp.close().unwrap();
    }

    #[test]
    fn clear_timeout() {
        let len = 10;
        let timeout = time::Duration::new(0, 100_000_000); // 100ms

        let tmp = tempdir::TempDir::new("clear_timeout").unwrap();
        let fifo_path = tmp.path().join("fifo");
        unistd::mkfifo(&fifo_path, Mode::from_bits(0o666).unwrap()).unwrap();
        let mut reader = Reader::from_files(
            vec![&fifo_path],
            len,
            WhenEOF::Close,
            Some(timeout),
        ).unwrap();
        let mut fifo = fs::OpenOptions::new().write(true).open(&fifo_path).unwrap();

        let thread = thread::spawn(move || {
            // Read a full frame, it should not contain data from the first partially sent frame.
            let mut rd_buf = vec![0; len];
            reader.read_exact(&mut rd_buf).unwrap();
            assert_eq!(vec![2; len], rd_buf);
        });

        // Send a partial frame over the fifo.
        copy_iter(&mut fifo, iter::repeat(1).take(len - 1));

        // Wait for the clear timeout to expire and the partial frame to be discarded.
        thread::sleep(timeout * 2);
        // Send a full frame over the fifo.
        copy_iter(&mut fifo, iter::repeat(2).take(len));

        thread.join().unwrap();
    }
}

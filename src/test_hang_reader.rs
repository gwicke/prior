#[cfg(test)]
mod test_hang {
    use std::io::Read;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn test_reader_handle_eof() {
        let expr = duct::cmd!("echo", "hello world");
        let handle = expr
            .stderr_to_stdout()
            .unchecked()
            .reader()
            .expect("reader");
        let handle = Arc::new(handle);

        let mut buf = [0u8; 8192];
        let mut out = Vec::new();
        let mut reader: &duct::ReaderHandle = &handle;
        let mut total = 0usize;
        let start = Instant::now();

        loop {
            if start.elapsed() > Duration::from_secs(5) {
                panic!("TIMEOUT after {} bytes", total);
            }
            match (&mut reader).read(&mut buf) {
                Ok(0) => {
                    println!("EOF after {} bytes (elapsed: {:?})", total, start.elapsed());
                    break;
                }
                Ok(n) => {
                    total += n;
                    out.extend_from_slice(&buf[..n]);
                }
                Err(e) => {
                    println!(
                        "Read error after {} bytes: {} (elapsed: {:?})",
                        total,
                        e,
                        start.elapsed()
                    );
                    break;
                }
            }
        }

        println!("Total: {} bytes", total);
        println!("Output: {:?}", String::from_utf8_lossy(&out));
        assert!(total > 0, "Should have read some output");
    }

    #[test]
    fn test_reader_handle_sleep() {
        let expr = duct::cmd!("sleep", "1");
        let handle = expr
            .stderr_to_stdout()
            .unchecked()
            .reader()
            .expect("reader");
        let handle = Arc::new(handle);

        let mut buf = [0u8; 8192];
        let mut reader: &duct::ReaderHandle = &handle;
        let start = Instant::now();

        loop {
            if start.elapsed() > Duration::from_secs(10) {
                panic!("TIMEOUT after {:?}", start.elapsed());
            }
            match (&mut reader).read(&mut buf) {
                Ok(0) => {
                    println!("EOF after {:?}", start.elapsed());
                    break;
                }
                Ok(n) => {
                    println!("Read {} bytes after {:?}", n, start.elapsed());
                }
                Err(e) => {
                    println!("Read error: {} after {:?}", e, start.elapsed());
                    break;
                }
            }
        }
    }

    #[test]
    fn test_reader_handle_bash_sleep() {
        let expr = duct::cmd!("bash", "-c", "echo hello; sleep 0.5; echo world");
        let handle = expr
            .stderr_to_stdout()
            .unchecked()
            .reader()
            .expect("reader");
        let handle = Arc::new(handle);

        let mut buf = [0u8; 8192];
        let mut reader: &duct::ReaderHandle = &handle;
        let start = Instant::now();

        loop {
            if start.elapsed() > Duration::from_secs(10) {
                panic!("TIMEOUT after {:?}", start.elapsed());
            }
            match (&mut reader).read(&mut buf) {
                Ok(0) => {
                    println!("EOF after {:?}", start.elapsed());
                    break;
                }
                Ok(n) => {
                    println!(
                        "Read {} bytes after {:?}: {:?}",
                        n,
                        start.elapsed(),
                        String::from_utf8_lossy(&buf[..n])
                    );
                }
                Err(e) => {
                    println!("Read error: {} after {:?}", e, start.elapsed());
                    break;
                }
            }
        }
    }
}

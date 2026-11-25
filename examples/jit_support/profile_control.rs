use std::io::{self, Read, Write};

pub struct Control<W, R> {
    commands: W,
    acknowledgements: R,
    acknowledged: bool,
}

impl<W: Write, R: Read> Control<W, R> {
    pub fn new(commands: W, acknowledgements: R) -> Self {
        Self {
            commands,
            acknowledgements,
            acknowledged: false,
        }
    }

    fn command(&mut self, command: &[u8]) -> io::Result<()> {
        self.commands.write_all(command)?;
        self.commands.flush()?;
        let mut ack = [0; 4];
        self.acknowledgements.read_exact(&mut ack[..1])?;
        if self.acknowledged && ack[0] == 0 {
            self.acknowledgements.read_exact(&mut ack[..1])?;
        }
        self.acknowledgements.read_exact(&mut ack[1..])?;
        if ack != *b"ack\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid perf control acknowledgement",
            ));
        }
        self.acknowledged = true;
        Ok(())
    }

    pub fn enabled<T, E: From<io::Error>>(
        &mut self,
        run: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.command(b"enable\n")?;
        let result = run();
        self.command(b"disable\n")?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn enables_then_disables_on_success_and_workload_error() {
        for fail in [false, true] {
            let mut control = Control::new(Vec::new(), &b"ack\nack\n"[..]);
            let called = Cell::new(false);
            let result = control.enabled(|| {
                called.set(true);
                if fail {
                    Err(io::Error::other("workload failed"))
                } else {
                    Ok(42)
                }
            });
            assert!(called.get());
            assert_eq!(result.is_err(), fail);
            assert_eq!(control.commands, b"enable\ndisable\n");
            assert!(control.acknowledgements.is_empty());
        }
    }

    #[test]
    fn refuses_invalid_or_truncated_enable_ack_before_running() {
        for ack in [&b""[..], &b"ack"[..], &b"bad\n"[..]] {
            let mut control = Control::new(Vec::new(), ack);
            let result: io::Result<()> = control.enabled(|| panic!("not acknowledged"));
            assert!(result.is_err());
            assert_eq!(control.commands, b"enable\n");
        }
    }

    #[test]
    fn refuses_missing_disable_ack_after_running() {
        let mut control = Control::new(Vec::new(), &b"ack\n"[..]);
        let result: io::Result<()> = control.enabled(|| Ok(()));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(control.commands, b"enable\ndisable\n");
    }

    #[test]
    fn accepts_optional_nul_terminated_acknowledgements_across_windows() {
        for ack in [
            &b"ack\nack\nack\nack\n"[..],
            &b"ack\n\0ack\n\0ack\n\0ack\n\0"[..],
            &b"ack\n\0ack\nack\n\0ack\n"[..],
        ] {
            let mut control = Control::new(Vec::new(), ack);
            for _ in 0..2 {
                let result: io::Result<()> = control.enabled(|| Ok(()));
                result.unwrap();
            }
            assert_eq!(control.commands, b"enable\ndisable\nenable\ndisable\n");
        }
    }

    #[test]
    fn refuses_leading_or_repeated_nul_padding() {
        let mut control = Control::new(Vec::new(), &b"\0ack\n"[..]);
        let result: io::Result<()> = control.enabled(|| panic!("invalid leading NUL"));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut control = Control::new(Vec::new(), &b"ack\n\0\0ack\n"[..]);
        let result: io::Result<()> = control.enabled(|| Ok(()));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn command_write_failure_prevents_workload_execution() {
        struct Refuse;
        impl Write for Refuse {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed control"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut control = Control::new(Refuse, &b"ack\n"[..]);
        let result: io::Result<()> = control.enabled(|| panic!("command failed"));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }
}

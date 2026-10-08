use crate::{event::InternalEvent, Command};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialOrd, PartialEq, Eq, Hash)]
pub enum ClipboardLocation {
    #[default]
    Clipboard,
    Primary,
}

#[derive(Clone, Debug, PartialOrd, PartialEq, Eq, Hash)]
pub enum ClipboardEvent {
    Start {
        location: ClipboardLocation,
        password: Option<String>,
        id: Option<String>,
    },
    Data {
        mime: String,
        bytes: Vec<u8>,
        id: Option<String>,
    },
    Done {
        id: Option<String>,
    },
    Error {
        status: String,
        id: Option<String>,
    },
}

pub struct ClipboardResponse<'a>(pub &'a ClipboardEvent);

impl Command for ClipboardResponse<'_> {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b]5522;type=read")?;
        let id = match self.0 {
            ClipboardEvent::Start {
                location,
                password,
                id,
            } => {
                f.write_str(":status=OK")?;
                if *location == ClipboardLocation::Primary {
                    f.write_str(":loc=primary")?;
                }
                if let Some(password) = password {
                    write!(f, ":pw={}", STANDARD.encode(password))?;
                }
                id
            }
            ClipboardEvent::Data { mime, id, .. } => {
                write!(f, ":status=DATA:mime={}", STANDARD.encode(mime))?;
                id
            }
            ClipboardEvent::Done { id } => {
                f.write_str(":status=DONE")?;
                id
            }
            ClipboardEvent::Error { status, id } => {
                if !matches!(status.as_str(), "ENOSYS" | "EPERM" | "EBUSY") {
                    return Err(fmt::Error);
                }
                write!(f, ":status={status}")?;
                id
            }
        };
        if let Some(id) = id {
            if id
                .bytes()
                .any(|c| !c.is_ascii_alphanumeric() && !b"-_+.".contains(&c))
            {
                return Err(fmt::Error);
            }
            write!(f, ":id={id}")?;
        }
        if let ClipboardEvent::Data { bytes, .. } = self.0 {
            if bytes.len() > 4096 {
                return Err(fmt::Error);
            }
            write!(f, ";{}", STANDARD.encode(bytes))?;
        }
        f.write_str("\x1b\\")
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MIME clipboard requires ANSI support",
        ))
    }
}

pub struct ClipboardRead<'a> {
    pub location: ClipboardLocation,
    pub mime_types: &'a [&'a str],
    pub password: Option<&'a str>,
    pub name: Option<&'a str>,
    pub id: Option<&'a str>,
}

impl Command for ClipboardRead<'_> {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b]5522;type=read")?;
        if self.location == ClipboardLocation::Primary {
            f.write_str(":loc=primary")?;
        }
        for (key, value) in [("pw", self.password), ("name", self.name)] {
            if let Some(value) = value {
                write!(f, ":{key}={}", STANDARD.encode(value))?;
            }
        }
        if let Some(id) = self.id {
            if id
                .bytes()
                .any(|c| !c.is_ascii_alphanumeric() && !b"-_+.".contains(&c))
            {
                return Err(fmt::Error);
            }
            write!(f, ":id={id}")?;
        }
        write!(f, ";{}\x1b\\", STANDARD.encode(self.mime_types.join(" ")))
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MIME clipboard requires ANSI support",
        ))
    }
}

macro_rules! mode_command {
    ($name:ident, $sequence:literal) => {
        pub struct $name;
        impl Command for $name {
            fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
                f.write_str($sequence)
            }
            #[cfg(windows)]
            fn execute_winapi(&self) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "MIME paste requires ANSI support",
                ))
            }
        }
    };
}
mode_command!(EnableMimePaste, "\x1b[?5522h");
mode_command!(DisableMimePaste, "\x1b[?5522l");
mode_command!(QueryMimePaste, "\x1b[?5522$p");

pub(crate) fn parse(buffer: &[u8]) -> std::io::Result<Option<InternalEvent>> {
    ClipboardEvent::parse_ansi(buffer)
        .map(|event| event.map(|event| InternalEvent::Event(crate::event::Event::Clipboard(event))))
}

impl ClipboardEvent {
    pub fn parse_ansi(buffer: &[u8]) -> std::io::Result<Option<Self>> {
        let end = if buffer.ends_with(b"\x1b\\") {
            buffer.len() - 2
        } else if buffer.ends_with(b"\x07") {
            buffer.len() - 1
        } else {
            if buffer.len() > 16 * 1024 {
                return Err(invalid());
            }
            return Ok(None);
        };
        let body = buffer.get(2..end).ok_or_else(invalid)?;
        let mut fields = body.splitn(3, |c| *c == b';');
        if fields.next() != Some(b"5522".as_slice()) {
            return Err(invalid());
        }
        let metadata =
            std::str::from_utf8(fields.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
        let mut status = None;
        let mut reading = false;
        let mut password = None;
        let mut location = ClipboardLocation::Clipboard;
        let mut mime = None;
        let mut id = None;
        for field in metadata.split(':') {
            let (key, value) = field.split_once('=').ok_or_else(invalid)?;
            match key {
                "type" => reading = value == "read",
                "status" => status = Some(value),
                "loc" => {
                    location = match value {
                        "clipboard" => ClipboardLocation::Clipboard,
                        "primary" => ClipboardLocation::Primary,
                        _ => return Err(invalid()),
                    }
                }
                "pw" => {
                    password = Some(
                        String::from_utf8(STANDARD.decode(value).map_err(|_| invalid())?)
                            .map_err(|_| invalid())?,
                    )
                }
                "mime" => {
                    mime = Some(
                        String::from_utf8(STANDARD.decode(value).map_err(|_| invalid())?)
                            .map_err(|_| invalid())?,
                    )
                }
                "id" => {
                    if value
                        .bytes()
                        .any(|c| !c.is_ascii_alphanumeric() && !b"-_+.".contains(&c))
                    {
                        return Err(invalid());
                    }
                    id = Some(value.into());
                }
                _ => (),
            }
        }
        if !reading {
            return Err(invalid());
        }
        let event = match status.ok_or_else(invalid)? {
            "OK" => ClipboardEvent::Start {
                location,
                password,
                id,
            },
            "DATA" => ClipboardEvent::Data {
                mime: mime.ok_or_else(invalid)?,
                bytes: STANDARD
                    .decode(fields.next().unwrap_or_default())
                    .map_err(|_| invalid())?,
                id,
            },
            "DONE" => ClipboardEvent::Done { id },
            status => ClipboardEvent::Error {
                status: status.into(),
                id,
            },
        };
        Ok(Some(event))
    }
}

fn invalid() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid clipboard response",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_notifications_and_read_commands_preserve_the_selection() {
        let packet = b"\x1b]5522;type=read:status=OK:loc=primary:pw=c2VjcmV0\x1b\\";
        let event = ClipboardEvent::parse_ansi(packet)
            .expect("notification")
            .expect("event");
        assert_eq!(
            event,
            ClipboardEvent::Start {
                location: ClipboardLocation::Primary,
                password: Some("secret".into()),
                id: None,
            }
        );
        let mut notification = String::new();
        ClipboardResponse(&event)
            .write_ansi(&mut notification)
            .expect("notification");
        assert_eq!(notification.as_bytes(), packet);
        let mut request = String::new();
        ClipboardRead {
            location: ClipboardLocation::Primary,
            mime_types: &["text/plain"],
            password: Some("secret"),
            name: Some("Paste event"),
            id: None,
        }
        .write_ansi(&mut request)
        .expect("request");
        assert_eq!(request, "\x1b]5522;type=read:loc=primary:pw=c2VjcmV0:name=UGFzdGUgZXZlbnQ=;dGV4dC9wbGFpbg==\x1b\\");
        assert!(
            ClipboardEvent::parse_ansi(b"\x1b]5522;type=read:status=OK:loc=other\x1b\\").is_err()
        );
    }
    #[test]
    fn mime_packets_are_typed_and_fragmentation_waits_for_the_terminator() {
        let packet = b"\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;AP8B\x1b\\";
        for length in 3..packet.len() {
            assert!(parse(&packet[..length]).expect("partial packet").is_none());
        }
        assert_eq!(
            parse(packet).expect("data"),
            Some(InternalEvent::Event(crate::event::Event::Clipboard(
                ClipboardEvent::Data {
                    mime: "image/png".into(),
                    bytes: vec![0, 255, 1],
                    id: None
                }
            )))
        );
        assert!(parse(b"\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;!bad\x07").is_err());
    }
    #[test]
    fn commands_use_the_kitty_password_keys_and_padded_base64() {
        let mut encoded = String::new();
        ClipboardRead {
            location: ClipboardLocation::Clipboard,
            mime_types: &["image/png"],
            password: Some("secret"),
            name: Some("Paste event"),
            id: None,
        }
        .write_ansi(&mut encoded)
        .expect("command");
        assert_eq!(
            encoded,
            "\x1b]5522;type=read:pw=c2VjcmV0:name=UGFzdGUgZXZlbnQ=;aW1hZ2UvcG5n\x1b\\"
        );
    }
}

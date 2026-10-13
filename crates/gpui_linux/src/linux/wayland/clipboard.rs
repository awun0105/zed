use std::{
    fs::File,
    io::{self, ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
};

use calloop::{LoopHandle, PostAction};
use filedescriptor::Pipe;
use strum::IntoEnumIterator;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::{
    WaylandClientStatePtr,
    platform::{PIPE_READ_TIMEOUT, read_fd_with_timeout},
};
use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat, hash};
use url::Url;

/// Text mime types that we'll offer to other programs.
pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const FILE_LIST_MIME_TYPE: &str = "text/uri-list";
pub(crate) const GNOME_COPY_FILES_MIME_TYPE: &str = "x-special/gnome-copied-files";

/// Text mime types that we'll accept from other programs.
pub(crate) const ALLOWED_TEXT_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "UTF8_STRING"];

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    self_mime: String,

    // Internal clipboard
    contents: Option<ClipboardItem>,
    primary_contents: Option<ClipboardItem>,

    // External clipboard
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<WlDataOffer>>,
    cached_primary_read: Option<ClipboardItem>,
    current_primary_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|t| t == mime_type)
    }

    fn read_bytes(&self, connection: &Connection, mime_type: &str) -> Option<Vec<u8>> {
        let pipe = Pipe::new().unwrap();
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        connection.flush().unwrap();

        match read_fd_with_timeout(fd, PIPE_READ_TIMEOUT) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                log::error!("error reading clipboard pipe: {err:?}");
                None
            }
        }
    }

    fn read_text(&self, connection: &Connection) -> Option<ClipboardItem> {
        let mime_type = self.mime_types.iter().find(|&mime_type| {
            ALLOWED_TEXT_MIME_TYPES
                .iter()
                .any(|&allowed| allowed == mime_type)
        })?;
        let bytes = self.read_bytes(connection, mime_type)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                log::error!("Failed to convert clipboard content to UTF-8: {}", e);
                return None;
            }
        };

        // Normalize the text to unix line endings, otherwise
        // copying from eg: firefox inserts a lot of blank
        // lines, and that is super annoying.
        let result = text_content.replace("\r\n", "\n");
        Some(ClipboardItem::new_string(result))
    }

    fn read_image(&self, connection: &Connection) -> Option<ClipboardItem> {
        for format in ImageFormat::iter() {
            let mime_type = format.mime_type();
            if !self.has_mime_type(mime_type) {
                continue;
            }

            if let Some(bytes) = self.read_bytes(connection, mime_type) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }
}

impl Clipboard {
    pub fn new(
        connection: Connection,
        loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    ) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime: format!("pid/{}", std::process::id()),

            contents: None,
            primary_contents: None,

            cached_read: None,
            current_offer: None,
            cached_primary_read: None,
            current_primary_offer: None,
        }
    }

    pub fn set(&mut self, item: ClipboardItem) {
        self.contents = Some(item);
    }

    pub fn set_primary(&mut self, item: ClipboardItem) {
        self.primary_contents = Some(item);
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.cached_read = None;
        self.current_offer = data_offer;
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.cached_primary_read = None;
        self.current_primary_offer = data_offer;
    }

    pub fn self_mime(&self) -> String {
        self.self_mime.clone()
    }

    pub(crate) fn mime_types(&self, item: &ClipboardItem) -> anyhow::Result<Vec<String>> {
        let has_text = item
            .entries()
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::String(_)));
        let has_image = item.entries().iter().any(
            |entry| matches!(entry, ClipboardEntry::Image(image) if !image.bytes().is_empty()),
        );
        let has_files = item.entries().iter().any(|entry| {
            matches!(entry, ClipboardEntry::ExternalPaths(paths) if !paths.paths().is_empty())
        });

        if !has_text && !has_image && !has_files {
            anyhow::bail!("clipboard item has no transferable data");
        }
        if has_files {
            file_uris(item)?;
        }

        let mut mime_types = Vec::new();
        if has_text {
            mime_types.extend(
                TEXT_MIME_TYPES
                    .iter()
                    .map(|mime_type| (*mime_type).to_owned()),
            );
        }
        for entry in item.entries() {
            if let ClipboardEntry::Image(image) = entry
                && !image.bytes().is_empty()
            {
                mime_types.push(image.format().mime_type().to_owned());
            }
        }
        if has_files {
            mime_types.push(FILE_LIST_MIME_TYPE.to_owned());
            mime_types.push(GNOME_COPY_FILES_MIME_TYPE.to_owned());
        }
        mime_types.push(self.self_mime());
        Ok(mime_types)
    }

    pub fn send(&self, mime_type: String, fd: OwnedFd) {
        let Some(contents) = self.contents.as_ref() else {
            return;
        };
        match Self::bytes_for_mime(contents, &mime_type) {
            Ok(Some(bytes)) => self.send_internal(fd, bytes),
            Ok(None) => log::warn!("No clipboard payload is available for MIME type {mime_type}"),
            Err(error) => log::error!("Failed to prepare clipboard MIME type {mime_type}: {error}"),
        }
    }

    pub fn send_primary(&self, mime_type: String, fd: OwnedFd) {
        let Some(contents) = self.primary_contents.as_ref() else {
            return;
        };
        match Self::bytes_for_mime(contents, &mime_type) {
            Ok(Some(bytes)) => self.send_internal(fd, bytes),
            Ok(None) => {
                log::warn!("No primary clipboard payload is available for MIME type {mime_type}")
            }
            Err(error) => {
                log::error!("Failed to prepare primary clipboard MIME type {mime_type}: {error}")
            }
        }
    }

    fn bytes_for_mime(item: &ClipboardItem, mime_type: &str) -> anyhow::Result<Option<Vec<u8>>> {
        if TEXT_MIME_TYPES.contains(&mime_type) {
            return Ok(text_bytes(item));
        }
        if mime_type == FILE_LIST_MIME_TYPE {
            return Ok(Some(file_uris(item)?.join("\r\n").into_bytes()));
        }
        if mime_type == GNOME_COPY_FILES_MIME_TYPE {
            return Ok(Some(
                format!("copy\n{}", file_uris(item)?.join("\n")).into_bytes(),
            ));
        }
        for entry in item.entries() {
            if let ClipboardEntry::Image(image) = entry
                && image.format().mime_type() == mime_type
            {
                return Ok(Some(image.bytes().to_vec()));
            }
        }
        Ok(None)
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_offer.as_ref()?;
        if let Some(cached) = self.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.contents.clone();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_primary_offer.as_ref()?;
        if let Some(cached) = self.cached_primary_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.primary_contents.clone();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_primary_read = Some(item.clone());
        Some(item)
    }

    fn send_internal(&self, fd: OwnedFd, bytes: Vec<u8>) {
        if let Err(error) = set_nonblocking(&fd) {
            log::error!("Failed to set Wayland clipboard pipe nonblocking: {error}");
            drop(fd);
            return;
        }
        let mut written = 0;
        if let Err(error) = self.loop_handle.insert_source(
            calloop::generic::Generic::new(
                File::from(fd),
                calloop::Interest::WRITE,
                calloop::Mode::Level,
            ),
            move |_, file, _| {
                let file = unsafe { file.get_mut() };
                loop {
                    match file.write(&bytes[written..]) {
                        Ok(n) if written + n == bytes.len() => {
                            written += n;
                            break Ok(PostAction::Remove);
                        }
                        Ok(0) => break Ok(PostAction::Remove),
                        Ok(n) => written += n,
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(err) if err.kind() == ErrorKind::WouldBlock => {
                            break Ok(PostAction::Continue);
                        }
                        Err(err) if err.kind() == ErrorKind::WriteZero => {
                            break Ok(PostAction::Remove);
                        }
                        Err(_) => break Ok(PostAction::Remove),
                    }
                }
            },
        ) {
            log::error!("Failed to schedule Wayland clipboard transfer: {error}");
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw_fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn text_bytes(item: &ClipboardItem) -> Option<Vec<u8>> {
    let mut text = String::new();
    let mut has_text = false;
    for entry in item.entries() {
        if let ClipboardEntry::String(value) = entry {
            has_text = true;
            text.push_str(value.text());
        }
    }
    has_text.then(|| text.into_bytes())
}

fn file_uris(item: &ClipboardItem) -> anyhow::Result<Vec<String>> {
    let mut uris = Vec::new();
    for entry in item.entries() {
        if let ClipboardEntry::ExternalPaths(paths) = entry {
            for path in paths.paths() {
                let uri = Url::from_file_path(path).map_err(|_| {
                    anyhow::anyhow!("cannot convert {} to a file URI", path.display())
                })?;
                uris.push(uri.to_string());
            }
        }
    }
    if uris.is_empty() {
        anyhow::bail!("clipboard file list is empty");
    }
    Ok(uris)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn file_uris_percent_encode_paths() {
        let item = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                smallvec::smallvec![
                    PathBuf::from("/tmp/clip board/#café.txt"),
                    PathBuf::from("/tmp/second.txt")
                ],
            ))],
        };

        assert_eq!(
            file_uris(&item).unwrap(),
            vec![
                "file:///tmp/clip%20board/%23caf%C3%A9.txt".to_owned(),
                "file:///tmp/second.txt".to_owned()
            ]
        );
    }

    #[test]
    fn image_payload_uses_its_declared_mime() {
        let bytes = vec![1, 2, 3];
        let image = Image::from_bytes(ImageFormat::Png, bytes.clone());
        let item = ClipboardItem::new_image(&image);

        assert_eq!(
            Clipboard::bytes_for_mime(&item, "image/png").unwrap(),
            Some(bytes)
        );
        assert_eq!(
            Clipboard::bytes_for_mime(&item, "image/jpeg").unwrap(),
            None
        );
    }

    #[test]
    fn gnome_payload_contains_all_file_uris() {
        let item = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                smallvec::smallvec![PathBuf::from("/tmp/one.txt"), PathBuf::from("/tmp/two.txt")],
            ))],
        };

        assert_eq!(
            String::from_utf8(
                Clipboard::bytes_for_mime(&item, GNOME_COPY_FILES_MIME_TYPE)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            "copy\nfile:///tmp/one.txt\nfile:///tmp/two.txt"
        );
    }
}

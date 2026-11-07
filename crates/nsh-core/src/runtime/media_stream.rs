//! `nsh`'s `MediaStream`: a live stream the embedding app registered by
//! name, which `<Video srcObject>` plays ([`crate::element::MediaStream`]).
//! It holds only the name: the embedding app owns the stream, its frames
//! never reach JS.

use std::{rc::Rc, sync::Arc};

use crate::element::{ClassInstance, Instance, JsError, class};

/// A live stream (a call's video, a camera) the embedding app registered,
/// for a video's `srcObject`.
#[class]
pub(crate) struct MediaStream {
    stream: crate::element::MediaStream,
}

#[class]
impl MediaStream {
    /// The stream the embedding app registered as `name` (on the web, the
    /// page with `player.registerStream`). A name nothing registered shows
    /// nothing (logged once). A name that is empty or longer than 256 bytes
    /// is a `TypeError`.
    pub(crate) fn from_host(name: String) -> Result<MediaStream, JsError> {
        crate::element::MediaStream::checked(&name).map_err(JsError::Type)?;
        Ok(MediaStream {
            stream: crate::element::MediaStream::from_host(name),
        })
    }

    /// The name the embedding app registered the stream with.
    #[getter]
    pub(crate) fn name(&self) -> String {
        self.stream.name().to_owned()
    }
}

/// The stream's name, when `instance` is a `MediaStream`.
pub(crate) fn name_of(instance: Rc<dyn ClassInstance>) -> Option<Arc<str>> {
    Instance::<MediaStream>::of(instance).map(|stream| stream.with(|s| s.stream.name_arc()))
}

//! Contact photos for avatars ([`Avatar::photo`](crate::components::Avatar::photo)),
//! one cache for the whole app. The app installs a loader once ([`set_photo_loader`]:
//! the synced Google and Microsoft photos and, when turned on, Gravatar); views ask
//! [`contact_photo`] while they render, get `None` (the initials) until the photo is
//! there, and redraw when photos arrive ([`observe_contact_photos`]).
//!
//! Each address is loaded once, off the main thread and a few at a time, until
//! [`refresh_contact_photos`] (contacts or the settings changed): then each is loaded
//! again when it is shown next, and the old photo stays until the new one is there.
//! Without a loader (tests, the demo) every avatar shows initials.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use gpui::{
    App, AsyncApp, BorrowAppContext as _, Context, Global, Image, ImageFormat, ImageSource,
    Subscription,
};

/// A photo as the loader returns it.
#[derive(Clone, PartialEq, Eq)]
pub struct PhotoData {
    /// `image/jpeg`, `image/png`, …
    pub content_type: String,
    /// The image file.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for PhotoData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotoData")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Loads the photo of an address. Runs off the main thread and may block (the
/// encrypted cache, the network).
pub type PhotoLoader = Arc<dyn Fn(&str) -> Option<PhotoData> + Send + Sync>;

/// How many photos load at once.
const PARALLEL: usize = 4;

/// What the cache knows about an address; each carries the photo shown meanwhile.
enum Entry {
    /// Being loaded.
    Loading(Option<ImageSource>),
    /// Loaded (`None`: there is none).
    Loaded(Option<ImageSource>),
    /// Loaded before a refresh: shown, and loaded again when asked for.
    Stale(Option<ImageSource>),
}

#[derive(Default)]
struct ContactPhotos {
    loader: Option<PhotoLoader>,
    /// By lowercase address.
    entries: RefCell<HashMap<String, Entry>>,
    queue: RefCell<VecDeque<String>>,
    /// A task works through the queue.
    draining: Cell<bool>,
    /// Bumped by a refresh: loads that started before it are dropped.
    generation: Cell<u64>,
}

impl Global for ContactPhotos {}

impl ContactPhotos {
    fn refresh(&mut self) {
        self.generation.set(self.generation.get() + 1);
        self.queue.get_mut().clear();
        for entry in self.entries.get_mut().values_mut() {
            let (Entry::Loading(photo) | Entry::Loaded(photo) | Entry::Stale(photo)) = entry;
            *entry = Entry::Stale(photo.take());
        }
    }
}

/// Installs the loader the avatars get their photos from, and forgets the photos
/// loaded so far.
pub fn set_photo_loader(
    cx: &mut App,
    loader: impl Fn(&str) -> Option<PhotoData> + Send + Sync + 'static,
) {
    let loader: PhotoLoader = Arc::new(loader);
    if cx.has_global::<ContactPhotos>() {
        cx.update_global::<ContactPhotos, _>(|photos, _| {
            photos.loader = Some(loader);
            photos.refresh();
        });
    } else {
        cx.set_global(ContactPhotos {
            loader: Some(loader),
            ..ContactPhotos::default()
        });
    }
}

/// The photo of `email` for its avatar, if one is loaded; asks the loader for it
/// otherwise, and the views observing the photos redraw once it is there.
pub fn contact_photo(email: &str, cx: &App) -> Option<ImageSource> {
    let photos = cx.try_global::<ContactPhotos>()?;
    photos.loader.as_ref()?;
    let key = email.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    let shown = {
        let mut entries = photos.entries.borrow_mut();
        let shown = match entries.get(&key) {
            Some(Entry::Loaded(photo) | Entry::Loading(photo)) => return photo.clone(),
            Some(Entry::Stale(photo)) => photo.clone(),
            None => None,
        };
        entries.insert(key.clone(), Entry::Loading(shown.clone()));
        shown
    };
    photos.queue.borrow_mut().push_back(key);
    if !photos.draining.replace(true) {
        cx.foreground_executor()
            .spawn(drain(cx.to_async()))
            .detach();
    }
    shown
}

/// Loads every photo again when it is shown next (contacts or the settings changed);
/// the old ones stay until then.
pub fn refresh_contact_photos(cx: &mut App) {
    if cx.has_global::<ContactPhotos>() {
        cx.update_global::<ContactPhotos, _>(|photos, _| photos.refresh());
    }
}

/// Redraws the view whenever photos arrived or were refreshed.
pub fn observe_contact_photos<V: 'static>(cx: &mut Context<'_, V>) -> Subscription {
    cx.observe_global::<ContactPhotos>(|_, cx| cx.notify())
}

/// Works through the queue, [`PARALLEL`] photos at a time, until it is empty.
async fn drain(cx: AsyncApp) {
    loop {
        let (batch, loader, generation) = cx.update(|cx| {
            let photos = cx.global::<ContactPhotos>();
            let mut queue = photos.queue.borrow_mut();
            let batch: Vec<String> = std::iter::from_fn(|| queue.pop_front())
                .take(PARALLEL)
                .collect();
            if batch.is_empty() || photos.loader.is_none() {
                photos.draining.set(false);
            }
            (batch, photos.loader.clone(), photos.generation.get())
        });
        let Some(loader) = loader.filter(|_| !batch.is_empty()) else {
            return;
        };
        let loads: Vec<_> = batch
            .into_iter()
            .map(|email| {
                let loader = loader.clone();
                cx.background_executor().spawn(async move {
                    let photo = loader(&email);
                    (email, photo)
                })
            })
            .collect();
        let mut loaded = Vec::with_capacity(loads.len());
        for load in loads {
            loaded.push(load.await);
        }
        cx.update_global::<ContactPhotos, _>(|photos, _| {
            if photos.generation.get() != generation {
                return;
            }
            let mut entries = photos.entries.borrow_mut();
            for (email, photo) in loaded {
                entries.insert(email, Entry::Loaded(photo.and_then(image)));
            }
        });
    }
}

/// The image of a loaded photo; `None` for a type GPUI can't show.
fn image(photo: PhotoData) -> Option<ImageSource> {
    let format = ImageFormat::from_mime_type(&photo.content_type)?;
    Some(ImageSource::Image(Arc::new(Image::from_bytes(
        format,
        photo.bytes,
    ))))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gpui::TestAppContext;

    use super::*;

    /// A loader with a photo for Anna only, recording what it was asked.
    fn install(cx: &mut TestAppContext) -> Arc<Mutex<Vec<String>>> {
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = asked.clone();
        cx.update(|cx| {
            set_photo_loader(cx, move |email| {
                log.lock().unwrap().push(email.to_owned());
                (email == "anna@example.com").then(|| PhotoData {
                    content_type: "image/png".to_owned(),
                    bytes: b"png".to_vec(),
                })
            });
        });
        asked
    }

    fn photo(email: &str, cx: &mut TestAppContext) -> Option<ImageSource> {
        cx.update(|cx| contact_photo(email, cx))
    }

    #[gpui::test]
    fn without_a_loader_there_are_no_photos(cx: &mut TestAppContext) {
        assert!(photo("anna@example.com", cx).is_none());
        cx.run_until_parked();
        assert!(photo("anna@example.com", cx).is_none());
    }

    #[gpui::test]
    fn each_address_loads_once_and_observers_hear_of_it(cx: &mut TestAppContext) {
        let asked = install(cx);
        let heard = Arc::new(Mutex::new(0));
        let count = heard.clone();
        let _observer = cx
            .update(|cx| cx.observe_global::<ContactPhotos>(move |_| *count.lock().unwrap() += 1));

        // Not loaded yet: the initials; then the photo, and the observers redraw.
        assert!(photo(" Anna@Example.com", cx).is_none());
        assert!(photo("bob@example.net", cx).is_none());
        cx.run_until_parked();
        assert!(*heard.lock().unwrap() > 0);
        assert!(matches!(
            photo("anna@example.com", cx),
            Some(ImageSource::Image(_))
        ));
        assert!(photo("bob@example.net", cx).is_none());
        cx.run_until_parked();
        // Loaded in parallel, each once.
        let mut asked = asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, ["anna@example.com", "bob@example.net"]);
    }

    #[gpui::test]
    fn a_refresh_loads_again_and_shows_the_old_photo_meanwhile(cx: &mut TestAppContext) {
        let asked = install(cx);
        photo("anna@example.com", cx);
        cx.run_until_parked();
        cx.update(refresh_contact_photos);
        assert!(photo("anna@example.com", cx).is_some(), "still shown");
        cx.run_until_parked();
        assert!(photo("anna@example.com", cx).is_some());
        assert_eq!(asked.lock().unwrap().len(), 2);
    }
}

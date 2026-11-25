//! The thumbnail pipeline: which files to decode, in what order, and the
//! cache in front of the result.
//!
//! Decoding is the most expensive thing a folder view does, so none of it runs
//! on the event loop. [ThumbScheduler] owns the queue and the panels' visible
//! windows, the worker threads pull jobs from it, and the UI side only ever
//! publishes a whole new request set or reports a viewport change.
//!
//! The ordering is what keeps a fast scroll usable: rows on screen right now
//! must win over rows that were on screen a moment ago, and both must win over
//! the rest of the folder, so a scroll is never stuck behind a folder-wide
//! decode sweep. [ThumbLru] is a separate and much cheaper concern: a decoded
//! image is kept by path and reused until an operation invalidates it.
//!
//! Everything here is `pub(super)` because bridge.rs still owns [AppState] and
//! the few call sites that feed this pipeline.

use super::*;

/// Decoding job for a thumbnail, handed to the background worker.
pub(super) struct ThumbJob {
    pub(super) path: PathBuf,
    pub(super) kind: FileKind,
    /// SVGs are loaded by Slint (resvg) on the event loop; other
    /// formats go through `generate_thumb` on the worker.
    pub(super) svg: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ThumbLocation {
    pub(super) panel: usize,
    pub(super) row: usize,
    pub(super) row_count: usize,
}

pub(super) struct ThumbRequest {
    pub(super) job: ThumbJob,
    pub(super) locations: Vec<ThumbLocation>,
}

struct ScheduledThumb {
    job: ThumbJob,
    locations: Vec<ThumbLocation>,
    serial: i32,
}

struct InFlightThumb {
    locations: Vec<ThumbLocation>,
    serial: i32,
}

pub(super) struct ThumbWork {
    pub(super) job: ThumbJob,
    pub(super) serial: i32,
}

impl std::ops::Deref for ThumbWork {
    type Target = ThumbJob;

    fn deref(&self) -> &Self::Target {
        &self.job
    }
}

/// Sort key of the queue. Visible rows come before the window's
/// neighborhood, which itself comes before the rest of the folder. `distance` stabilizes the order
/// around the visible zone; `panel` and `row` make ties deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ThumbPriority {
    tier: u8,
    distance: usize,
    panel: usize,
    row: usize,
}

#[derive(Default)]
pub(super) struct ThumbQueue {
    pending: HashMap<PathBuf, ScheduledThumb>,
    pub(super) ready: BinaryHeap<Reverse<(ThumbPriority, i32, PathBuf)>>,
    /// Current locations of an in-progress decode. They can be enriched
    /// if another view displays the same path while the worker is working.
    in_flight: HashMap<PathBuf, InFlightThumb>,
    next_serial: i32,
}

/// Priority queue shared with the worker. Unlike a FIFO channel,
/// it can promote newly visible rows and remove paths
/// from a folder that was left before their decoding starts.
pub(super) struct ThumbScheduler {
    pub(super) queue: Mutex<ThumbQueue>,
    /// Small state independent of the heavy queue: the scroll callback can
    /// never afford to wait for a heap of thousands of entries to be rebuilt.
    viewports: Mutex<HashMap<usize, (usize, usize)>>,
    pub(super) priorities_dirty: AtomicBool,
    wake: Condvar,
    started: AtomicBool,
}

impl ThumbScheduler {
    pub(super) fn new() -> Self {
        Self {
            queue: Mutex::new(ThumbQueue::default()),
            viewports: Mutex::new(HashMap::new()),
            priorities_dirty: AtomicBool::new(false),
            wake: Condvar::new(),
            started: AtomicBool::new(false),
        }
    }

    pub(super) fn start_once(&self) -> bool {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn priority_for(
        scheduled: &ScheduledThumb,
        viewports: &HashMap<usize, (usize, usize)>,
    ) -> ThumbPriority {
        scheduled
            .locations
            .iter()
            .map(|loc| {
                let fallback = ThumbPriority {
                    tier: 2,
                    distance: loc.row,
                    panel: loc.panel,
                    row: loc.row,
                };
                let Some(&(raw_first, raw_last)) = viewports.get(&loc.panel) else {
                    return fallback;
                };
                if loc.row_count == 0 {
                    return fallback;
                }
                let first = raw_first.min(loc.row_count - 1);
                let last = raw_last.max(first).min(loc.row_count - 1);
                if loc.row >= first && loc.row <= last {
                    return ThumbPriority {
                        tier: 0,
                        distance: loc.row - first,
                        panel: loc.panel,
                        row: loc.row,
                    };
                }
                let distance = if loc.row < first {
                    first - loc.row
                } else {
                    loc.row - last
                };
                // Preloads one viewport height on each side. This
                // absorbs a normal scroll without delaying a far-away destination.
                let visible_len = last - first + 1;
                ThumbPriority {
                    tier: if distance <= visible_len { 1 } else { 2 },
                    distance,
                    panel: loc.panel,
                    row: loc.row,
                }
            })
            .min()
            .unwrap_or(ThumbPriority {
                tier: 2,
                distance: usize::MAX,
                panel: usize::MAX,
                row: usize::MAX,
            })
    }

    fn rebuild_ready(queue: &mut ThumbQueue, viewports: &HashMap<usize, (usize, usize)>) {
        queue.ready = queue
            .pending
            .iter()
            .map(|(path, scheduled)| {
                Reverse((
                    Self::priority_for(scheduled, viewports),
                    scheduled.serial,
                    path.clone(),
                ))
            })
            .collect();
    }

    pub(super) fn ensure_ready(&self, queue: &mut ThumbQueue) {
        // An update can arrive during the rebuild. The loop then re-reads
        // the most recent viewport before letting the next job go.
        while self.priorities_dirty.swap(false, Ordering::AcqRel) {
            let viewports = self
                .viewports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            Self::rebuild_ready(queue, &viewports);
        }
    }

    /// Replaces the full request with that of the panels currently in
    /// preview mode. Paths shared across several views are decoded
    /// only once, while keeping each of their positions for sorting.
    pub(super) fn replace_pending(&self, requests: Vec<ThumbRequest>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::mem::take(&mut queue.pending);
        let mut next: HashMap<PathBuf, ScheduledThumb> = HashMap::new();

        for request in requests {
            let path = request.job.path.clone();
            if let Some(in_flight) = queue.in_flight.get_mut(&path) {
                for location in request.locations {
                    if !in_flight.locations.contains(&location) {
                        in_flight.locations.push(location);
                    }
                }
                continue;
            }
            if let Some(existing) = next.get_mut(&path) {
                existing.locations.extend(request.locations);
                continue;
            }
            let serial = old.get(&path).map(|s| s.serial).unwrap_or_else(|| {
                let serial = queue.next_serial;
                queue.next_serial = queue.next_serial.wrapping_add(1).max(1);
                serial
            });
            next.insert(
                path,
                ScheduledThumb {
                    job: request.job,
                    locations: request.locations,
                    serial,
                },
            );
        }

        queue.pending = next;
        queue.ready.clear();
        self.priorities_dirty.store(true, Ordering::Release);
        drop(queue);
        self.wake.notify_all();
    }

    /// Adds a few requests that became visible without re-scanning/rebuilding
    /// the whole gallery. Paths already pending/in-flight are simply
    /// enriched with their new location.
    pub(super) fn merge_pending(&self, requests: Vec<ThumbRequest>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut priorities_changed = false;
        for request in requests {
            let path = request.job.path.clone();
            if let Some(in_flight) = queue.in_flight.get_mut(&path) {
                for location in request.locations {
                    if !in_flight.locations.contains(&location) {
                        in_flight.locations.push(location);
                    }
                }
                continue;
            }
            if let Some(scheduled) = queue.pending.get_mut(&path) {
                for location in request.locations {
                    if !scheduled.locations.contains(&location) {
                        scheduled.locations.push(location);
                        priorities_changed = true;
                    }
                }
                continue;
            }
            let serial = queue.next_serial;
            queue.next_serial = queue.next_serial.wrapping_add(1).max(1);
            queue.pending.insert(
                path,
                ScheduledThumb {
                    job: request.job,
                    locations: request.locations,
                    serial,
                },
            );
            priorities_changed = true;
        }
        if priorities_changed {
            queue.ready.clear();
            self.priorities_dirty.store(true, Ordering::Release);
        }
        drop(queue);
        if priorities_changed {
            self.wake.notify_all();
        }
    }

    /// Updates the only piece of information that varies during a scroll. No
    /// filesystem access or Slint model access happens here.
    pub(super) fn update_viewport(&self, panel: usize, first: i32, last: i32) {
        let mut viewports = self.viewports.lock().unwrap_or_else(|e| e.into_inner());
        let changed = if first < 0 || last < first {
            viewports.remove(&panel).is_some()
        } else {
            let range = (first as usize, last as usize);
            if viewports.get(&panel).copied() == Some(range) {
                false
            } else {
                viewports.insert(panel, range);
                true
            }
        };
        drop(viewports);
        if changed {
            // The O(n) recomputation is deliberately deferred to the worker: the
            // scroll callback stays O(1), regardless of the folder's size.
            self.priorities_dirty.store(true, Ordering::Release);
            self.wake.notify_all();
        }
    }

    fn take_next(&self) -> ThumbWork {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            self.ensure_ready(&mut queue);
            while let Some(Reverse((_, _, path))) = queue.ready.pop() {
                if let Some(scheduled) = queue.pending.remove(&path) {
                    let serial = scheduled.serial;
                    queue.in_flight.insert(
                        path,
                        InFlightThumb {
                            locations: scheduled.locations,
                            serial,
                        },
                    );
                    return ThumbWork {
                        job: scheduled.job,
                        serial,
                    };
                }
            }
            queue = self.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(test)]
    pub(super) fn try_take_next(&self) -> Option<ThumbWork> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_ready(&mut queue);
        while let Some(Reverse((_, _, path))) = queue.ready.pop() {
            if let Some(scheduled) = queue.pending.remove(&path) {
                let serial = scheduled.serial;
                queue.in_flight.insert(
                    path,
                    InFlightThumb {
                        locations: scheduled.locations,
                        serial,
                    },
                );
                return Some(ThumbWork {
                    job: scheduled.job,
                    serial,
                });
            }
        }
        None
    }

    /// Acknowledges a UI success or a decode failure, then frees the worker.
    pub(super) fn complete(&self, path: &Path, serial: i32) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue
            .in_flight
            .get(path)
            .is_some_and(|in_flight| in_flight.serial == serial)
        {
            queue.in_flight.remove(path);
        }
        drop(queue);
        self.wake.notify_all();
    }

    fn wait_until_complete(&self, path: &Path, serial: i32) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        while queue
            .in_flight
            .get(path)
            .is_some_and(|in_flight| in_flight.serial == serial)
        {
            queue = self.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(super) fn in_flight_locations(&self, path: &Path, serial: i32) -> Vec<ThumbLocation> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
            .get(path)
            .filter(|in_flight| in_flight.serial == serial)
            .map(|in_flight| in_flight.locations.clone())
            .unwrap_or_default()
    }

    /// Drops queued/in-flight generations for paths whose contents changed.
    /// A worker may still finish an old decode, but its serial can no longer
    /// match a later request for the same path, so the UI safely discards it.
    pub(super) fn invalidate_paths(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for path in paths {
            changed |= queue.pending.remove(path).is_some();
            changed |= queue.in_flight.remove(path).is_some();
        }
        if changed {
            queue.ready.clear();
            self.priorities_dirty.store(true, Ordering::Release);
        }
        drop(queue);
        if changed {
            self.wake.notify_all();
        }
    }
}

/// **In-memory** LRU cache (full path → image), bounded by entry count.
/// Session only: nothing is written to disk (privacy constraint).
// Paths whose contents may have changed are evicted explicitly, while
// unrelated folders stay hot.
pub(super) struct ThumbLru {
    map: HashMap<String, Image>,
    order: VecDeque<String>,
    cap: usize,
}

impl ThumbLru {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
    }
    pub(super) fn get(&mut self, key: &str) -> Option<Image> {
        let img = self.map.get(key).cloned()?;
        self.touch(key);
        Some(img)
    }
    /// Test without cloning or promoting the image. Global scans can thus
    /// skip off-screen rows already in cache without skewing LRU recency:
    /// only textures actually reused on screen call `get`.
    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
    pub(super) fn put(&mut self, key: String, img: Image) {
        if !self.map.contains_key(&key)
            && self.map.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.map.remove(&old);
        }
        self.map.insert(key.clone(), img);
        self.touch(&key);
    }

    pub(super) fn remove_path(&mut self, path: &Path) {
        let key = path.to_string_lossy();
        self.map.remove(key.as_ref());
        if let Some(position) = self.order.iter().position(|item| item == key.as_ref()) {
            self.order.remove(position);
        }
    }
}

/// Invalidates only the content paths touched by an operation or watcher
/// event. Untouched thumbnails stay hot in the bounded LRU. The scheduler is
/// invalidated at the same time so an older asynchronous decode cannot restore
/// a stale texture after a path has been renamed or replaced.
pub(super) fn invalidate_thumbnail_paths(state: &AppState, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let unique: HashSet<PathBuf> = paths.iter().cloned().collect();
    {
        let mut cache = state.thumb_cache.borrow_mut();
        for path in &unique {
            cache.remove_path(path);
        }
    }
    let paths: Vec<PathBuf> = unique.into_iter().collect();
    state.thumb_scheduler.invalidate_paths(&paths);
}

pub(super) fn drain_thumbnail_invalidations(state: &AppState) -> bool {
    let paths: Vec<PathBuf> = state
        .thumb_invalidations
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .collect();
    let changed = !paths.is_empty();
    invalidate_thumbnail_paths(state, &paths);
    changed
}

pub(super) fn active_thumbnail_paths(state: &AppState) -> Vec<PathBuf> {
    let active = *state.active_panel.borrow();
    let panels = state.panels.borrow();
    let Some(panel) = panels.get(active) else {
        return Vec::new();
    };
    (0..panel.rows_model.row_count())
        .filter_map(|index| panel.rows_model.row_data(index))
        // `.lnk` rows can preview an image/audio/video/PDF target even though the
        // shortcut itself is not classified as preview-capable.
        .filter(|row| row.preview_capable || row.ext.eq_ignore_ascii_case("lnk"))
        .map(|row| row_path(&row))
        .collect()
}

/// Builds a `slint::Image` from a decoded thumbnail (RGBA8).
pub(super) fn image_from_thumb(t: &Thumbnail) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(t.width, t.height);
    buf.make_mut_bytes().copy_from_slice(&t.rgba);
    Image::from_rgba8(buf)
}

/// Selects the thumbnail source based on the platform and type. On
/// **Windows**, audio/video/PDF fallbacks go through native APIs
/// (`winthumb`): OS providers → standalone (no ffmpeg/poppler) and
/// **no console window**. Images remain decoded in Rust (`image`), while model
/// previews use embedded images, optional 7-Zip decompression, or CPU mesh rendering.
/// On **Linux**, everything goes through `thumbnail::generate` (images and
/// MP3/FLAC covers are in-process; video/PDF use their best-effort CLIs).
pub(super) fn generate_thumb(path: &Path, kind: FileKind, max_px: u32) -> Option<Thumbnail> {
    #[cfg(windows)]
    {
        // A `.lnk` shortcut → the shell thumbnail API on the link itself: the
        // shell resolves the target and returns ITS thumbnail (an image shortcut
        // previews the image) or its icon, exactly like Explorer.
        if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            return crate::winthumb::shell_thumbnail(path, max_px);
        }
        match kind {
            // MP3/FLAC: embedded cover first; the shell can supply artwork for
            // tags/providers outside Favnyr's deliberately small parsers.
            FileKind::Audio => {
                return thumbnail::generate(path, kind, max_px)
                    .or_else(|| crate::winthumb::shell_thumbnail(path, max_px));
            }
            // Video: shell thumbnail API (frame via OS providers).
            FileKind::Video => return crate::winthumb::shell_thumbnail(path, max_px),
            // PDF: Windows's built-in PDF engine (WinRT) → no external binary.
            FileKind::Document => return crate::winthumb::pdf_thumbnail(path, max_px),
            // Image: Rust decoding first (common formats plus bounded embedded
            // PSD/Affinity previews); on failure, fall back to the shell API.
            FileKind::Image => {
                return thumbnail::generate(path, kind, max_px)
                    .or_else(|| crate::winthumb::shell_thumbnail(path, max_px));
            }
            FileKind::File => {
                return thumbnail::generate(path, kind, max_px)
                    .or_else(|| crate::winthumb::shell_thumbnail(path, max_px));
            }
            _ => {}
        }
    }
    thumbnail::generate(path, kind, max_px)
}

/// Size of the thumbnail worker pool. The dominant cost (image decoding, or
/// launching ffmpeg/shell) is CPU-bound and parallelizable per file. Capped at
/// **4**: beyond that, applying the textures (single UI thread), memory
/// bandwidth, and I/O cap the gain, a visible folder shows only
/// ~15 rows, and N simultaneous full-resolution decodes bound the peak
/// memory (~4x the peak of a single image). We also leave one core for the UI thread on
/// small machines (`- 1`). Safe fallback to 1 if introspection fails.
pub(super) fn thumb_worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .saturating_sub(1)
        .clamp(1, 4)
}

/// Background worker: picks the best job at the moment it becomes available,
/// then waits for it to be applied by the event loop before picking the next one.
/// This way, a scroll that happens during decoding immediately influences that choice.
/// Several instances run in parallel (see `thumb_worker_count`): each
/// takes a DISTINCT job (the scheduler removes the path from `pending` under
/// the lock) and waits only for ITS OWN completion — the others keep decoding.
pub(super) fn spawn_thumb_worker(scheduler: Arc<ThumbScheduler>, weak: slint::Weak<MainWindow>) {
    // 256 px: covers the tallest row height at max zoom (≈220 px) without blur.
    const MAX_PX: u32 = 256;
    std::thread::spawn(move || {
        loop {
            let work = scheduler.take_next();
            let serial = work.serial;
            let job = work.job;
            let wait_path = job.path.clone();
            let path_str = job.path.to_string_lossy().to_string();
            let weak_for_ui = weak.clone();
            let scheduler_for_ui = scheduler.clone();

            if job.svg {
                let ui_path = job.path;
                let posted = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_ui.upgrade() {
                        match Image::load_from_path(&ui_path) {
                            Ok(img) => w.invoke_thumb_ready(path_str.into(), serial, img),
                            Err(_) => scheduler_for_ui.complete(&ui_path, serial),
                        }
                    } else {
                        scheduler_for_ui.complete(&ui_path, serial);
                    }
                });
                if posted.is_err() {
                    scheduler.complete(&wait_path, serial);
                } else {
                    scheduler.wait_until_complete(&wait_path, serial);
                }
                continue;
            }

            let Some(thumb) = generate_thumb(&job.path, job.kind, MAX_PX) else {
                scheduler.complete(&wait_path, serial);
                continue;
            };
            let completion_path = wait_path.clone();
            let posted = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_ui.upgrade() {
                    w.invoke_thumb_ready(path_str.into(), serial, image_from_thumb(&thumb));
                } else {
                    scheduler_for_ui.complete(&completion_path, serial);
                }
            });
            if posted.is_err() {
                scheduler.complete(&wait_path, serial);
            } else {
                scheduler.wait_until_complete(&wait_path, serial);
            }
        }
    });
}

/// For each panel in preview mode: applies the cached thumbnails to
/// image/audio-cover/video/PDF/SVG/model rows, then atomically replaces the request for the
/// missing ones. To be called after a (re)population of fresh rows.
pub(super) fn thumbnail_kind_for_row(kind: i32, ext: &str) -> Option<(FileKind, bool)> {
    let svg = kind == 6 && ext.eq_ignore_ascii_case("svg");
    let file_kind = match kind {
        4 if ext.eq_ignore_ascii_case("mp3") || ext.eq_ignore_ascii_case("flac") => FileKind::Audio,
        6 => FileKind::Image,
        7 => FileKind::Video,
        5 if ext.eq_ignore_ascii_case("pdf") => FileKind::Document,
        1 if thumbnail::is_model_preview_extension(ext) => FileKind::File,
        _ => return None,
    };
    Some((file_kind, svg))
}

/// Merges a local request before touching the shared scheduler. The same
/// image can appear in several panels; all its locations are
/// kept, but the decoding stays unique.
pub(super) fn push_thumbnail_request(
    requests: &mut HashMap<PathBuf, ThumbRequest>,
    path: PathBuf,
    kind: FileKind,
    svg: bool,
    location: ThumbLocation,
) {
    if let Some(existing) = requests.get_mut(&path) {
        if !existing.locations.contains(&location) {
            existing.locations.push(location);
        }
    } else {
        requests.insert(
            path.clone(),
            ThumbRequest {
                job: ThumbJob { path, kind, svg },
                locations: vec![location],
            },
        );
    }
}

pub(super) fn request_thumbnails(state: &AppState) {
    let mut requests: HashMap<PathBuf, ThumbRequest> = HashMap::new();
    let panels = state.panels.borrow();
    for (panel_idx, panel) in panels.iter().enumerate() {
        let tab = &panel.tabs.tabs[panel.tabs.active];
        if !tab.preview {
            continue;
        }
        let model = &panel.rows_model;
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else {
                continue;
            };
            // Search can return thousands of entries from a large tree. Decode
            // only its render window; viewport changes request newly seen rows.
            if panel.search.is_some() && !row.rendered {
                continue;
            }
            if row.thumbnail.size().width > 0 {
                if row.rendered {
                    continue; // texture already attached to a visible row
                }
                // A rebuilt geometry may have briefly kept a
                // an off-screen texture. We detach it, then actually check
                // the LRU instead of assuming it still lives there.
                row.thumbnail = Image::default();
                model.set_row_data(i, row.clone());
            }
            let full = row_path(&row);
            let key = full.to_string_lossy().to_string();

            // Shared source of truth with the row geometry: only these
            // types are capable of receiving a content texture.
            let (kind, svg) = match thumbnail_kind_for_row(row.kind, row.ext.as_str()) {
                Some(k) => k,
                // A `.lnk` to an image/audio/video/PDF previews the TARGET (the shell
                // thumbnail API resolves the link). Only `.lnk` rows pay the lookup.
                None if row.ext.eq_ignore_ascii_case("lnk") => match lnk_thumbnail_kind(&full) {
                    Some(kind) => (kind, false),
                    None => continue,
                },
                None => continue,
            };
            if row.rendered {
                if let Some(img) = state.thumb_cache.borrow_mut().get(&key) {
                    row.thumbnail = img;
                    model.set_row_data(i, row);
                    continue;
                }
            } else if state.thumb_cache.borrow().contains(&key) {
                // Don't clone/promote every off-screen image: they
                // remain available, but LRU recency reflects only the actually
                // visible usages. Above all, no unnecessary re-decoding.
                continue;
            }
            push_thumbnail_request(
                &mut requests,
                full,
                kind,
                svg,
                ThumbLocation {
                    panel: panel_idx,
                    row: i,
                    row_count: model.row_count(),
                },
            );
        }
    }
    drop(panels);
    state
        .thumb_scheduler
        .replace_pending(requests.into_values().collect());
}

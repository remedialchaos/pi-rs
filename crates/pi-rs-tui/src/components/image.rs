use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex, MutexGuard};

use indexmap::IndexMap;

use crate::terminal_image::{
    CellDimensions, ImageDimensions, ImageProtocol, ImageRenderOptions, allocate_image_id,
    get_capabilities, get_cell_dimensions, get_image_dimensions, get_png_dimensions,
    image_fallback, render_image,
};
use crate::{tui::Component, utils::truncate_with_ellipsis};

use super::TextStyleFn;

pub type ImageTranscoder = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

#[derive(Default)]
struct Transcoder {
    convert: Option<ImageTranscoder>,
    results: IndexMap<Arc<str>, Option<Arc<str>>>,
    revision: u64,
}

static TRANSCODER: std::sync::LazyLock<Mutex<Transcoder>> =
    std::sync::LazyLock::new(Mutex::default);

fn transcoder() -> MutexGuard<'static, Transcoder> {
    TRANSCODER.lock().unwrap_or_else(|error| error.into_inner())
}

pub fn set_image_transcoder(convert: Option<ImageTranscoder>) {
    let mut state = transcoder();
    state.convert = convert;
    state.results.clear();
    state.revision = state.revision.wrapping_add(1);
}

fn to_png(data: &Arc<str>, mime: &str) -> Option<Arc<str>> {
    let (convert, revision) = {
        let mut state = transcoder();
        if let Some(result) = state.results.shift_remove(data) {
            state.results.insert(data.clone(), result.clone());
            return result;
        }
        (state.convert.clone()?, state.revision)
    };
    let result = convert(data, mime).map(Arc::from);
    let mut state = transcoder();
    if state.revision == revision {
        state.results.insert(data.clone(), result.clone());
        if state.results.len() > 32 {
            state.results.shift_remove_index(0);
        }
    }
    result
}

#[derive(Default)]
pub struct ImageOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub filename: Option<String>,
    pub image_id: Option<u32>,
}

pub struct ImageTheme {
    pub fallback_color: TextStyleFn,
}

impl Default for ImageTheme {
    fn default() -> Self {
        Self {
            fallback_color: std::rc::Rc::new(str::to_owned),
        }
    }
}

struct ImageCache {
    width: usize,
    protocol: Option<ImageProtocol>,
    cell: CellDimensions,
    lines: Vec<String>,
}

pub struct Image {
    data: Arc<str>,
    mime: String,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: Cell<Option<u32>>,
    png: RefCell<Option<Arc<str>>>,
    cache: RefCell<Option<ImageCache>>,
}

impl Image {
    pub fn new(
        data: impl Into<Arc<str>>,
        mime: String,
        theme: ImageTheme,
        options: ImageOptions,
        dimensions: Option<ImageDimensions>,
    ) -> Self {
        let data = data.into();
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(&data, &mime))
            .unwrap_or(ImageDimensions {
                width_px: 800,
                height_px: 600,
            });
        let image_id = Cell::new(options.image_id);
        Self {
            data,
            mime,
            dimensions,
            theme,
            options,
            image_id,
            png: RefCell::new(None),
            cache: RefCell::new(None),
        }
    }

    pub fn image_id(&self) -> Option<u32> {
        self.image_id.get()
    }
}

impl Component for Image {
    fn invalidate(&mut self) {
        self.cache.get_mut().take();
    }
    fn render(&self, width: usize) -> Vec<String> {
        let protocol = get_capabilities().images;
        let cell = get_cell_dimensions();
        if let Some(cache) = self.cache.borrow().as_ref().filter(|cache| {
            cache.width == width && cache.cell == cell && cache.protocol == protocol
        }) {
            return cache.lines.clone();
        }
        let max_width = width
            .saturating_sub(2)
            .min(self.options.max_width_cells.unwrap_or(60))
            .max(1);
        let max_height = self.options.max_height_cells.unwrap_or_else(|| {
            max_width
                .saturating_mul(cell.width_px)
                .div_ceil(cell.height_px.max(1))
                .max(1)
        });
        let mut dimensions = self.dimensions;
        let mut png = self.png.borrow_mut();
        let data = if protocol == Some(ImageProtocol::Kitty) && self.mime != "image/png" {
            if png.is_none() {
                *png = to_png(&self.data, &self.mime);
            }
            png.as_deref().inspect(|data| {
                dimensions = get_png_dimensions(data).unwrap_or(dimensions);
            })
        } else {
            Some(self.data.as_ref())
        };
        let result = protocol.and_then(|protocol| {
            let data = data?;
            if protocol == ImageProtocol::Kitty && self.image_id().is_none() {
                self.image_id.set(Some(allocate_image_id()));
            }
            render_image(
                data,
                dimensions,
                ImageRenderOptions {
                    max_width_cells: Some(max_width),
                    max_height_cells: Some(max_height),
                    image_id: self.image_id(),
                    move_cursor: Some(false),
                    ..Default::default()
                },
            )
        });
        let lines = if let Some(image) = result {
            if image.image_id.is_some() {
                self.image_id.set(image.image_id);
            }
            let mut lines = vec![String::new(); image.rows.max(1)];
            if protocol == Some(ImageProtocol::Kitty) {
                lines[0] = image.sequence;
            } else {
                let offset = lines.len() - 1;
                lines[offset] = if offset == 0 {
                    image.sequence
                } else {
                    format!("\x1b[{offset}A{}", image.sequence)
                };
            }
            lines
        } else {
            let fallback = image_fallback(
                &self.mime,
                Some(self.dimensions),
                self.options.filename.as_deref(),
            );
            vec![truncate_with_ellipsis(
                &(self.theme.fallback_color)(&fallback),
                width,
                "…",
                false,
            )]
        };
        *self.cache.borrow_mut() = Some(ImageCache {
            width,
            protocol,
            cell,
            lines: lines.clone(),
        });
        lines
    }
}

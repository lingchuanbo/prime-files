//! PDF pages for the preview pane, rendered by Windows itself
//! (`Windows.Data.Pdf`, the engine Edge's predecessor used) on a background
//! thread, one page at a time so the first shows before the rest are done.

use gpui::{AppContext, Context, RenderImage, Task};
use std::sync::Arc;

use super::ExplorerPane;

/// Pages rendered before the rest is only counted: a 500-page manual would
/// otherwise hold hundreds of bitmaps for a glance at its cover.
const MAX_PAGES: u32 = 40;
/// Width pages are rendered at, in pixels: sharp in a preview pane at 2x.
const PAGE_WIDTH: u32 = 1100;

/// The pages of the PDF in the preview, as they arrive.
pub(crate) struct PdfPreview {
    pub pages: Vec<Arc<RenderImage>>,
    /// Pages in the document; `None` until it has been opened.
    pub total: Option<u32>,
    pub error: Option<String>,
    // Dropping it (another file previewed) stops the rendering.
    _task: Task<()>,
}

enum Update {
    Total(u32),
    Page(RenderImage),
    Failed(String),
}

impl ExplorerPane {
    /// Starts rendering the PDF at `path` into the preview.
    pub(crate) fn open_pdf_preview(&mut self, path: String, cx: &mut Context<Self>) {
        let (sender, receiver) = async_channel::unbounded();
        let worker = cx.background_spawn(async move { platform::render(&path, &sender) });
        let task = cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let alive = this.update(cx, |pane, cx| {
                    let Some(pdf) = pane.preview_pdf.as_mut() else {
                        return false;
                    };
                    match update {
                        Update::Total(total) => pdf.total = Some(total),
                        Update::Page(page) => pdf.pages.push(Arc::new(page)),
                        Update::Failed(error) => pdf.error = Some(error),
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
            worker.await;
        });
        self.preview_pdf = Some(PdfPreview {
            pages: Vec::new(),
            total: None,
            error: None,
            _task: task,
        });
        cx.notify();
    }

    /// Frees the GPU copies of the pages shown, when the preview moves on.
    pub(crate) fn close_pdf_preview(&mut self, cx: &mut gpui::App) {
        if let Some(pdf) = self.preview_pdf.take() {
            for page in pdf.pages {
                cx.drop_image(page, None);
            }
        }
    }
}

// The decoded page in the layout `RenderImage` wants: BGRA, premultiplied.
fn to_render_image(bytes: &[u8]) -> Option<RenderImage> {
    let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = decoded.dimensions();
    let mut bgra = decoded.into_raw();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        let premultiply = |channel: u8| ((u16::from(channel) * alpha + 127) / 255) as u8;
        let (red, green, blue) = (pixel[0], pixel[1], pixel[2]);
        pixel[0] = premultiply(blue);
        pixel[1] = premultiply(green);
        pixel[2] = premultiply(red);
    }
    super::thumbnails::render_image(width, height, bgra)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! `unsafe` only for joining COM on the worker thread.

    use super::{MAX_PAGES, PAGE_WIDTH, Update, to_render_image};
    use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
    use windows::Storage::StorageFile;
    use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::HSTRING;

    pub fn render(path: &str, sender: &async_channel::Sender<Update>) {
        // WinRT needs the thread in an apartment; an executor thread that
        // already is one keeps the one it has.
        //
        // The join is undone before returning: this runs on a pool thread that
        // other work reuses, and a thread left in the multithreaded apartment
        // made the next STA caller on it fail (the `trash` crate panicked on
        // exactly that and took the app down).
        let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        if let Err(error) = pages(path, sender) {
            sender
                .send_blocking(Update::Failed(error.message().to_string()))
                .ok();
        }
        if joined {
            unsafe { CoUninitialize() };
        }
    }

    fn pages(path: &str, sender: &async_channel::Sender<Update>) -> windows::core::Result<()> {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path))?.get()?;
        let document = PdfDocument::LoadFromFileAsync(&file)?.get()?;
        let total = document.PageCount()?;
        if sender.send_blocking(Update::Total(total)).is_err() {
            return Ok(());
        }
        for index in 0..total.min(MAX_PAGES) {
            let page = document.GetPage(index)?;
            let options = PdfPageRenderOptions::new()?;
            options.SetDestinationWidth(PAGE_WIDTH)?;
            let stream = InMemoryRandomAccessStream::new()?;
            page.RenderWithOptionsToStreamAsync(&stream, &options)?
                .get()?;
            let length = u32::try_from(stream.Size()?).unwrap_or(u32::MAX);
            let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
            reader.LoadAsync(length)?.get()?;
            let mut bytes = vec![0u8; length as usize];
            reader.ReadBytes(&mut bytes)?;
            let Some(image) = to_render_image(&bytes) else {
                continue;
            };
            // Nobody listening: the preview moved on to another file.
            if sender.send_blocking(Update::Page(image)).is_err() {
                return Ok(());
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Update;

    pub fn render(_path: &str, sender: &async_channel::Sender<Update>) {
        sender
            .send_blocking(Update::Failed(
                "PDF preview is only available on Windows".into(),
            ))
            .ok();
    }
}

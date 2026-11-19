//! The preview's picture of an image, decoded from the file itself at the
//! size the panel shows it.
//!
//! The preview used to show the Shell's thumbnail at that size. It comes from
//! the thumbnail cache, which Windows keeps as JPEG at a low quality, so a
//! photo looked blocky and smeared in the preview even when the file was
//! sharp (the owner's complaint, 05/11/2026). Now the file is decoded by
//! Windows Imaging Component, the codecs the Shell uses (so HEIC, WebP and
//! camera RAW with their extensions installed too), scaled with high-quality
//! cubic filtering straight to the panel's size and turned the way the photo
//! was taken. The Shell's picture still shows for the moment this takes.
//!
//! WIC's scaler pulls the image through in strips, and its JPEG decoder
//! scales while decoding: a 48 MP photo never sits in memory whole, which is
//! what the Shell's picture was brought in to avoid (OTIMIZACAO.md §3.5,
//! `docs/PERFORMANCE.md` passada 7).

/// An image decoded to fit in `edge` pixels, premultiplied BGRA, as
/// `(width, height, pixels)`. One no larger than that is decoded at its own
/// size. Runs on a thread of its own, with COM set up and torn down there.
pub(crate) fn decode_fitted(path: &str, edge: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let path = path.to_string();
    std::thread::Builder::new()
        .name("nohrs-picture".into())
        .spawn(move || platform::decode(&path, edge))
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|_| "the decoder crashed".to_string())?
}

/// The size an image of `width` x `height` is decoded at to fit in `edge`:
/// smaller, keeping its proportions, or its own when it already fits.
fn fitted(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= edge || longest == 0 {
        return (width, height);
    }
    let scale = edge as f64 / longest as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! WIC has no safe binding.

    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapSource,
        IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapInterpolationModeHighQualityCubic,
        WICBitmapPaletteTypeMedianCut, WICBitmapTransformFlipHorizontal, WICBitmapTransformOptions,
        WICBitmapTransformRotate0, WICBitmapTransformRotate90, WICBitmapTransformRotate180,
        WICBitmapTransformRotate270, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::core::{HSTRING, Interface, w};

    pub fn decode(path: &str, edge: u32) -> Result<(u32, u32, Vec<u8>), String> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let decoded = run(path, edge).map_err(|error| error.message().to_string());
        if initialized {
            unsafe { CoUninitialize() };
        }
        decoded
    }

    fn run(path: &str, edge: u32) -> windows::core::Result<(u32, u32, Vec<u8>)> {
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let decoder = factory.CreateDecoderFromFilename(
                &HSTRING::from(path),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )?;
            let frame = decoder.GetFrame(0)?;
            let (mut width, mut height) = (0, 0);
            frame.GetSize(&mut width, &mut height)?;
            // The photo's EXIF orientation: the pixels are stored as the
            // sensor saw them, turned for showing.
            let orientation = frame
                .GetMetadataQueryReader()
                .ok()
                .and_then(|reader| {
                    let mut value = PROPVARIANT::default();
                    reader
                        .GetMetadataByName(w!("System.Photo.Orientation"), &mut value)
                        .ok()?;
                    u16::try_from(&value).ok()
                })
                .unwrap_or(1);

            let mut source: IWICBitmapSource = frame.cast()?;
            let (fit_width, fit_height) = super::fitted(width, height, edge);
            if (fit_width, fit_height) != (width, height) {
                let scaler = factory.CreateBitmapScaler()?;
                scaler.Initialize(
                    &source,
                    fit_width,
                    fit_height,
                    WICBitmapInterpolationModeHighQualityCubic,
                )?;
                source = scaler.cast()?;
            }
            let turn = transform(orientation);
            if turn != WICBitmapTransformRotate0 {
                let rotator = factory.CreateBitmapFlipRotator()?;
                rotator.Initialize(&source, turn)?;
                source = rotator.cast()?;
            }
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                &source,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;
            let (mut out_width, mut out_height) = (0, 0);
            converter.GetSize(&mut out_width, &mut out_height)?;
            let stride = out_width * 4;
            let mut pixels = vec![0u8; (stride * out_height) as usize];
            converter.CopyPixels(std::ptr::null(), stride, &mut pixels)?;
            Ok((out_width, out_height, pixels))
        }
    }

    // EXIF orientation 1-8 as WIC's flip and rotation (2, 4, 5 and 7 are
    // mirrored).
    fn transform(orientation: u16) -> WICBitmapTransformOptions {
        let flip = WICBitmapTransformFlipHorizontal.0;
        WICBitmapTransformOptions(match orientation {
            2 => flip,
            3 => WICBitmapTransformRotate180.0,
            4 => WICBitmapTransformRotate180.0 | flip,
            5 => WICBitmapTransformRotate90.0 | flip,
            6 => WICBitmapTransformRotate90.0,
            7 => WICBitmapTransformRotate270.0 | flip,
            8 => WICBitmapTransformRotate270.0,
            _ => WICBitmapTransformRotate0.0,
        })
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn decode(_path: &str, _edge: u32) -> Result<(u32, u32, Vec<u8>), String> {
        Err("decoded by WIC on Windows only".into())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn a_large_image_is_fitted_and_a_small_one_kept() {
        assert_eq!(fitted(6000, 4000, 1024), (1024, 683));
        assert_eq!(fitted(4000, 6000, 1024), (683, 1024));
        assert_eq!(fitted(800, 600, 1024), (800, 600));
    }

    #[cfg(windows)]
    #[test]
    fn a_png_is_decoded_at_the_fitted_size_in_premultiplied_bgra() {
        let folder = tempfile::tempdir().expect("temp folder");
        let path = folder.path().join("a.png");
        // 400 x 200, opaque red.
        image::RgbaImage::from_pixel(400, 200, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .expect("save");
        let (width, height, pixels) = decode_fitted(&path.to_string_lossy(), 100).expect("decode");
        assert_eq!((width, height), (100, 50));
        assert_eq!(&pixels[..4], &[0, 0, 255, 255]);
    }

    // What WIC makes of a real file: NOHRS_PICTURE=<path> cargo test -- --ignored probe_picture
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn probe_picture() {
        let path = std::env::var("NOHRS_PICTURE").expect("NOHRS_PICTURE");
        let started = std::time::Instant::now();
        let decoded =
            decode_fitted(&path, 1024).map(|(width, height, pixels)| (width, height, pixels.len()));
        eprintln!("{decoded:?} in {:?}", started.elapsed());
    }
}

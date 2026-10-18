use std::borrow::Cow;

use image::{
    ExtendedColorType, ImageBuffer, ImageEncoder, Rgba,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::{self, FilterType},
};

use crate::{Encoded, Format, Image, Rect, ScreenError};

const JPEG_QUALITY: u8 = 80;

pub(crate) fn encode(
    image: &Image,
    region: Option<Rect>,
    max_edge: Option<u32>,
    format: Format,
) -> Result<Encoded, ScreenError> {
    if image.rgba.len() as u64 != u64::from(image.width) * u64::from(image.height) * 4 {
        return Err(ScreenError::Failed(format!(
            "{} bytes are not a {}x{} RGBA image",
            image.rgba.len(),
            image.width,
            image.height
        )));
    }
    let whole = Rect {
        x: 0,
        y: 0,
        width: image.width,
        height: image.height,
    };
    let crop = match region {
        None => whole,
        Some(region) => clamp(region, image.width, image.height).ok_or_else(|| {
            ScreenError::Failed(format!(
                "region {}x{} at {},{} is outside the {}x{} desktop",
                region.width, region.height, region.x, region.y, image.width, image.height
            ))
        })?,
    };
    let (width, height) = fit(crop.width, crop.height, max_edge);
    let cropped: Cow<[u8]> = if crop == whole {
        Cow::Borrowed(&image.rgba)
    } else {
        // clamp keeps the crop inside the image
        let stride = image.width as usize * 4;
        let (x, row) = (crop.x as usize * 4, crop.width as usize * 4);
        Cow::Owned(
            image
                .rgba
                .chunks_exact(stride)
                .skip(crop.y as usize)
                .take(crop.height as usize)
                .flat_map(|line| &line[x..x + row])
                .copied()
                .collect(),
        )
    };
    let pixels = if (width, height) == (crop.width, crop.height) {
        cropped
    } else {
        let view = ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(crop.width, crop.height, &cropped)
            .ok_or_else(|| ScreenError::Failed("cropping the screenshot".into()))?;
        Cow::Owned(imageops::resize(&view, width, height, FilterType::Triangle).into_raw())
    };

    let mut bytes = Vec::new();
    let written = match format {
        Format::Png => PngEncoder::new(&mut bytes).write_image(
            &pixels,
            width,
            height,
            ExtendedColorType::Rgba8,
        ),
        Format::Jpeg => {
            let rgb: Vec<u8> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[r, g, b, _]| [r, g, b])
                .collect();
            JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY).write_image(
                &rgb,
                width,
                height,
                ExtendedColorType::Rgb8,
            )
        }
    };
    written.map_err(|e| ScreenError::Failed(format!("encoding the screenshot: {e}")))?;
    Ok(Encoded {
        bytes,
        width,
        height,
        scale: f64::from(crop.width.max(crop.height)) / f64::from(width.max(height)),
        x: crop.x,
        y: crop.y,
    })
}

/// `region` ∩ the `width`×`height` image; `None` if empty
fn clamp(region: Rect, width: u32, height: u32) -> Option<Rect> {
    let span = |start: i32, len: u32, limit: u32| {
        let lo = i64::from(start).max(0);
        let hi = (i64::from(start) + i64::from(len)).min(i64::from(limit));
        (lo < hi).then_some((lo, hi - lo))
    };
    let (x, w) = span(region.x, region.width, width)?;
    let (y, h) = span(region.y, region.height, height)?;
    // both fit: x < width <= u32::MAX and w <= width, same for y
    Some(Rect {
        x: x as i32,
        y: y as i32,
        width: w as u32,
        height: h as u32,
    })
}

/// size with the longest edge at most `max_edge`, aspect kept, never upscaled
fn fit(width: u32, height: u32, max_edge: Option<u32>) -> (u32, u32) {
    let longest = width.max(height);
    let Some(max_edge) = max_edge.map(|m| m.max(1)).filter(|&m| m < longest) else {
        return (width, height);
    };
    let shorter = |side: u32| {
        let scaled =
            (u64::from(side) * u64::from(max_edge) + u64::from(longest) / 2) / u64::from(longest);
        (scaled as u32).max(1)
    };
    if width >= height {
        (max_edge, shorter(height))
    } else {
        (shorter(width), max_edge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pixel (x, y) = (x, y, x ^ y, 255)
    fn gradient(width: u32, height: u32) -> Image {
        let rgba = (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| [x as u8, y as u8, (x ^ y) as u8, 255]))
            .collect();
        Image {
            width,
            height,
            rgba,
        }
    }

    fn decode(encoded: &Encoded, format: Format) -> image::RgbaImage {
        let format = match format {
            Format::Png => image::ImageFormat::Png,
            Format::Jpeg => image::ImageFormat::Jpeg,
        };
        image::load_from_memory_with_format(&encoded.bytes, format)
            .unwrap()
            .into_rgba8()
    }

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn region_is_clamped_to_the_image() {
        let image = gradient(10, 8);
        let e = encode(&image, Some(rect(-2, 6, 5, 5)), None, Format::Png).unwrap();
        assert_eq!((e.x, e.y, e.width, e.height, e.scale), (0, 6, 3, 2, 1.0));
        let png = decode(&e, Format::Png);
        assert_eq!(png.get_pixel(0, 0).0, [0, 6, 6, 255]);
        assert_eq!(png.get_pixel(2, 1).0, [2, 7, 5, 255]);

        let e = encode(&image, Some(rect(7, -3, 100, 100)), None, Format::Png).unwrap();
        assert_eq!((e.x, e.y, e.width, e.height), (7, 0, 3, 8));
    }

    #[test]
    fn region_outside_the_image_fails() {
        let image = gradient(10, 8);
        for region in [
            rect(10, 0, 5, 5),
            rect(0, 8, 5, 5),
            rect(-5, 0, 5, 5),
            rect(2, 2, 0, 3),
            rect(i32::MAX, i32::MAX, u32::MAX, u32::MAX),
        ] {
            let Err(err) = encode(&image, Some(region), None, Format::Png) else {
                panic!("{region:?} encoded");
            };
            assert!(
                matches!(&err, ScreenError::Failed(m) if m.contains("outside the 10x8 desktop")),
                "{region:?}: {err}"
            );
        }
    }

    #[test]
    fn region_reaching_past_i32_max_is_clamped() {
        let e = encode(
            &gradient(4, 4),
            Some(rect(1, 1, u32::MAX, u32::MAX)),
            None,
            Format::Png,
        )
        .unwrap();
        assert_eq!((e.x, e.y, e.width, e.height), (1, 1, 3, 3));
    }

    #[test]
    fn downscale_bounds_the_longest_edge_and_reports_the_scale() {
        let e = encode(&gradient(400, 100), None, Some(100), Format::Png).unwrap();
        assert_eq!((e.width, e.height, e.scale), (100, 25, 4.0));
        let e = encode(&gradient(30, 90), None, Some(45), Format::Png).unwrap();
        assert_eq!((e.width, e.height, e.scale), (15, 45, 2.0));
        // rounding to the nearest pixel, never below one
        assert_eq!(fit(1000, 1, Some(10)), (10, 1));
        assert_eq!(fit(1000, 15, Some(100)), (100, 2));
        assert_eq!(fit(3, 1000, Some(0)), (1, 1));
        let e = encode(&gradient(256, 200), None, Some(100), Format::Png).unwrap();
        assert!(e.width.max(e.height) <= 100);
        assert_eq!(e.scale, 2.56);
    }

    #[test]
    fn never_upscales() {
        let e = encode(&gradient(10, 8), None, Some(100), Format::Png).unwrap();
        assert_eq!((e.width, e.height, e.scale), (10, 8, 1.0));
        let e = encode(&gradient(10, 8), None, Some(10), Format::Png).unwrap();
        assert_eq!((e.width, e.height, e.scale), (10, 8, 1.0));
    }

    #[test]
    fn crop_then_scale_reports_the_crop_origin() {
        let e = encode(
            &gradient(100, 100),
            Some(rect(20, 40, 60, 30)),
            Some(20),
            Format::Png,
        )
        .unwrap();
        assert_eq!(
            (e.x, e.y, e.width, e.height, e.scale),
            (20, 40, 20, 10, 3.0)
        );
    }

    #[test]
    fn both_formats_decode_to_the_reported_size() {
        let image = gradient(64, 48);
        for format in [Format::Png, Format::Jpeg] {
            for max_edge in [None, Some(20)] {
                let e = encode(&image, None, max_edge, format).unwrap();
                let decoded = decode(&e, format);
                assert_eq!((decoded.width(), decoded.height()), (e.width, e.height));
            }
        }
        let png = decode(
            &encode(&image, None, None, Format::Png).unwrap(),
            Format::Png,
        );
        assert_eq!(png.as_raw(), &image.rgba);
    }

    #[test]
    fn jpeg_drops_alpha() {
        let mut image = gradient(16, 16);
        image
            .rgba
            .iter_mut()
            .skip(3)
            .step_by(4)
            .for_each(|a| *a = 0);
        let e = encode(&image, None, None, Format::Jpeg).unwrap();
        let decoded =
            image::load_from_memory_with_format(&e.bytes, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgb8);
    }

    #[test]
    fn wrong_buffer_size_fails() {
        let image = Image {
            width: 4,
            height: 4,
            rgba: vec![0; 10],
        };
        assert!(matches!(
            encode(&image, None, None, Format::Png),
            Err(ScreenError::Failed(_))
        ));
    }
}

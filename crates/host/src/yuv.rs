//! RGB(A) to I420 for portal frames. Limited-range BT.601, matching what
//! libx264 and OpenH264 expect by default.

pub fn i420_size(width: u32, height: u32) -> usize {
    let w = width as usize;
    let h = height as usize;
    w * h + 2 * ((w / 2) * (h / 2))
}

pub fn even(value: u32) -> u32 {
    value & !1
}

pub fn packed_to_i420(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    order: PixelOrder,
) -> Option<Vec<u8>> {
    let width = even(width);
    let height = even(height);
    if width == 0 || height == 0 {
        return None;
    }
    let bpp = order.bytes_per_pixel();
    if stride < width as usize * bpp {
        return None;
    }
    let need = stride.saturating_mul(height as usize);
    if src.len() < need {
        return None;
    }
    let mut dst = vec![0u8; i420_size(width, height)];
    let y_size = width as usize * height as usize;
    let cw = width as usize / 2;
    let (y_plane, rest) = dst.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(cw * height as usize / 2);
    for row in 0..height as usize {
        for col in 0..width as usize {
            let o = row * stride + col * bpp;
            let (r, g, b) = order.rgb(&src[o..o + bpp]);
            let (y, u, v) = rgb_to_yuv(r, g, b);
            y_plane[row * width as usize + col] = y;
            if row % 2 == 0 && col % 2 == 0 {
                let c = (row / 2) * cw + col / 2;
                u_plane[c] = u;
                v_plane[c] = v;
            }
        }
    }
    Some(dst)
}

pub fn yuy2_to_i420(src: &[u8], stride: usize, width: u32, height: u32) -> Option<Vec<u8>> {
    let width = even(width);
    let height = even(height);
    if width == 0 || height == 0 || stride < width as usize * 2 {
        return None;
    }
    if src.len() < stride * height as usize {
        return None;
    }
    let mut dst = vec![0u8; i420_size(width, height)];
    let y_size = width as usize * height as usize;
    let cw = width as usize / 2;
    let (y_plane, rest) = dst.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(cw * height as usize / 2);
    for row in 0..height as usize {
        for col in 0..width as usize / 2 {
            let o = row * stride + col * 4;
            let y0 = src[o];
            let u = src[o + 1];
            let y1 = src[o + 2];
            let v = src[o + 3];
            let x = col * 2;
            y_plane[row * width as usize + x] = y0;
            y_plane[row * width as usize + x + 1] = y1;
            if row % 2 == 0 {
                let c = (row / 2) * cw + col;
                u_plane[c] = u;
                v_plane[c] = v;
            }
        }
    }
    Some(dst)
}

pub fn nv12_to_i420(
    src: &[u8],
    y_stride: usize,
    uv_stride: usize,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let width = even(width);
    let height = even(height);
    if width == 0 || height == 0 || y_stride < width as usize || uv_stride < width as usize {
        return None;
    }
    let y_bytes = y_stride * height as usize;
    let uv_bytes = uv_stride * (height as usize / 2);
    if src.len() < y_bytes + uv_bytes {
        return None;
    }
    let mut dst = vec![0u8; i420_size(width, height)];
    let y_size = width as usize * height as usize;
    let cw = width as usize / 2;
    let (y_plane, rest) = dst.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(cw * height as usize / 2);
    for row in 0..height as usize {
        let src_row = &src[row * y_stride..row * y_stride + width as usize];
        y_plane[row * width as usize..row * width as usize + width as usize]
            .copy_from_slice(src_row);
    }
    let uv = &src[y_bytes..];
    for row in 0..height as usize / 2 {
        for col in 0..cw {
            let o = row * uv_stride + col * 2;
            u_plane[row * cw + col] = uv[o];
            v_plane[row * cw + col] = uv[o + 1];
        }
    }
    Some(dst)
}

pub fn copy_i420(src: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let width = even(width);
    let height = even(height);
    let need = i420_size(width, height);
    if src.len() < need || width == 0 || height == 0 {
        return None;
    }
    Some(src[..need].to_vec())
}

#[derive(Clone, Copy)]
pub enum PixelOrder {
    Rgb,
    Bgr,
    Rgba,
    Bgra,
    Rgbx,
    Bgrx,
}

impl PixelOrder {
    fn bytes_per_pixel(self) -> usize {
        match self {
            PixelOrder::Rgb | PixelOrder::Bgr => 3,
            PixelOrder::Rgba | PixelOrder::Bgra | PixelOrder::Rgbx | PixelOrder::Bgrx => 4,
        }
    }

    fn rgb(self, px: &[u8]) -> (u8, u8, u8) {
        match self {
            PixelOrder::Rgb | PixelOrder::Rgba => (px[0], px[1], px[2]),
            PixelOrder::Bgr | PixelOrder::Bgra => (px[2], px[1], px[0]),
            PixelOrder::Rgbx => (px[0], px[1], px[2]),
            PixelOrder::Bgrx => (px[2], px[1], px[0]),
        }
    }
}

fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let r = i32::from(r);
    let g = i32::from(g);
    let b = i32::from(b);
    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (clamp_u8(y), clamp_u8(u), clamp_u8(v))
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

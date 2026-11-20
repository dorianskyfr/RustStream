//! Orientation correction for captured surfaces.
//!
//! Desktop Duplication returns the surface in the panel's native (unrotated)
//! orientation, with the desktop content rotated inside it, and reports the
//! output rotation in `DXGI_OUTDUPL_DESC::Rotation`. The image streamed to
//! the client (and the input mapping, which uses the desktop rectangle) must
//! be in desktop orientation, so the surface is rotated back while it is
//! copied out of the mapped texture.
//!
//! Mapping from a desktop pixel `(x, y)` to the surface pixel, for a surface
//! of `sw × sh` (same convention as the texture coordinates of Microsoft's
//! Desktop Duplication sample):
//!
//! | Rotation | Desktop size | Surface pixel |
//! |---|---|---|
//! | Identity | `sw × sh` | `(x, y)` |
//! | 90° | `sh × sw` | `(y, sh - 1 - x)` |
//! | 180° | `sw × sh` | `(sw - 1 - x, sh - 1 - y)` |
//! | 270° | `sh × sw` | `(sw - 1 - y, x)` |

use crate::even;

/// Rotation of the desktop relative to the captured surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Rotation {
    /// Desktop-oriented size of a `sw × sh` surface (before even cropping).
    pub fn oriented_size(self, sw: u32, sh: u32) -> (u32, u32) {
        match self {
            Self::Identity | Self::Rotate180 => (sw, sh),
            Self::Rotate90 | Self::Rotate270 => (sh, sw),
        }
    }
}

/// Copies a BGRA surface (`pitch` bytes per row, at least `sw * 4`) into
/// `dst` in desktop orientation, cropped to even dimensions, tightly packed.
/// Returns the output size. `src` must hold at least `pitch * (sh - 1) + sw * 4`
/// bytes.
pub fn copy_oriented(
    src: &[u8],
    pitch: usize,
    sw: u32,
    sh: u32,
    rotation: Rotation,
    dst: &mut Vec<u8>,
) -> (u32, u32) {
    let (ow, oh) = rotation.oriented_size(sw, sh);
    let (w, h) = (even(ow), even(oh));
    let (sw, sh, w, h) = (sw as usize, sh as usize, w as usize, h as usize);
    let row = w * 4;
    dst.resize(row * h, 0);
    if w == 0 || h == 0 {
        return (0, 0);
    }
    debug_assert!(pitch >= sw * 4 && src.len() >= pitch * (sh - 1) + sw * 4);
    match rotation {
        Rotation::Identity => {
            for (d, s) in dst.chunks_exact_mut(row).zip(src.chunks(pitch)) {
                d.copy_from_slice(&s[..row]);
            }
        }
        Rotation::Rotate180 => {
            for (y, d) in dst.chunks_exact_mut(row).enumerate() {
                let s = &src[(sh - 1 - y) * pitch..];
                for (x, px) in d.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let i = (sw - 1 - x) * 4;
                    px.copy_from_slice(&s[i..i + 4]);
                }
            }
        }
        Rotation::Rotate90 => {
            // Desktop row y reads surface column y, bottom to top.
            for (y, d) in dst.chunks_exact_mut(row).enumerate() {
                for (x, px) in d.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let i = (sh - 1 - x) * pitch + y * 4;
                    px.copy_from_slice(&src[i..i + 4]);
                }
            }
        }
        Rotation::Rotate270 => {
            // Desktop row y reads surface column sw-1-y, top to bottom.
            for (y, d) in dst.chunks_exact_mut(row).enumerate() {
                let col = (sw - 1 - y) * 4;
                for (x, px) in d.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let i = x * pitch + col;
                    px.copy_from_slice(&src[i..i + 4]);
                }
            }
        }
    }
    (w as u32, h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4×2 surface with padding (pitch 20); pixel value = its index, so each
    /// output pixel tells which surface pixel it came from.
    fn surface() -> (Vec<u8>, usize, u32, u32) {
        let (sw, sh, pitch) = (4u32, 2u32, 20usize);
        let mut src = vec![0xEE; pitch * sh as usize];
        for y in 0..sh as usize {
            for x in 0..sw as usize {
                let v = (y * sw as usize + x) as u8;
                src[y * pitch + x * 4..y * pitch + x * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        (src, pitch, sw, sh)
    }

    fn ids(buf: &[u8]) -> Vec<u8> {
        buf.as_chunks::<4>().0.iter().map(|p| p[0]).collect()
    }

    // Surface ids:
    //   0 1 2 3
    //   4 5 6 7
    #[test]
    fn identity_drops_padding() {
        let (src, pitch, sw, sh) = surface();
        let mut out = Vec::new();
        assert_eq!(
            copy_oriented(&src, pitch, sw, sh, Rotation::Identity, &mut out),
            (4, 2)
        );
        assert_eq!(ids(&out), [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn rotate_180() {
        let (src, pitch, sw, sh) = surface();
        let mut out = Vec::new();
        assert_eq!(
            copy_oriented(&src, pitch, sw, sh, Rotation::Rotate180, &mut out),
            (4, 2)
        );
        assert_eq!(ids(&out), [7, 6, 5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn rotate_90() {
        let (src, pitch, sw, sh) = surface();
        let mut out = Vec::new();
        assert_eq!(
            copy_oriented(&src, pitch, sw, sh, Rotation::Rotate90, &mut out),
            (2, 4)
        );
        // Desktop (x, y) <- surface (y, sh-1-x).
        assert_eq!(ids(&out), [4, 0, 5, 1, 6, 2, 7, 3]);
    }

    #[test]
    fn rotate_270() {
        let (src, pitch, sw, sh) = surface();
        let mut out = Vec::new();
        assert_eq!(
            copy_oriented(&src, pitch, sw, sh, Rotation::Rotate270, &mut out),
            (2, 4)
        );
        // Desktop (x, y) <- surface (sw-1-y, x).
        assert_eq!(ids(&out), [3, 7, 2, 6, 1, 5, 0, 4]);
    }

    #[test]
    fn rotations_compose() {
        // 90° then 270° (or 180° twice) restores the original image.
        let (src, pitch, sw, sh) = surface();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let (w, h) = copy_oriented(&src, pitch, sw, sh, Rotation::Rotate90, &mut a);
        copy_oriented(&a, w as usize * 4, w, h, Rotation::Rotate270, &mut b);
        assert_eq!(ids(&b), [0, 1, 2, 3, 4, 5, 6, 7]);
        let (w, h) = copy_oriented(&src, pitch, sw, sh, Rotation::Rotate180, &mut a);
        copy_oriented(&a, w as usize * 4, w, h, Rotation::Rotate180, &mut b);
        assert_eq!(ids(&b), [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn odd_sizes_are_cropped_to_even() {
        let (sw, sh, pitch) = (3u32, 5u32, 12usize);
        let src = vec![1u8; pitch * sh as usize];
        let mut out = Vec::new();
        assert_eq!(
            copy_oriented(&src, pitch, sw, sh, Rotation::Rotate90, &mut out),
            (4, 2)
        );
        assert_eq!(out.len(), 4 * 2 * 4);
    }
}

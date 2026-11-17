use std::time::{Duration, Instant};

use crate::{CaptureError, Frame, FrameSource, even};

/// Deterministic synthetic source: smooth gradients scrolling diagonally,
/// a fine moving stripe pattern and a sliding white block — every pixel
/// changes every frame, like a game camera panning. Paced at a fixed frame
/// rate: each call to `next_frame` produces a new frame once the frame
/// interval elapsed.
#[derive(Debug)]
pub struct TestPattern {
    width: u32,
    height: u32,
    interval: Duration,
    next_due: Instant,
    frame_no: u64,
    buf: Vec<u8>,
    last_at: Option<Instant>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        let (width, height) = (even(width), even(height));
        Self {
            width,
            height,
            interval: Duration::from_secs_f64(1.0 / f64::from(fps.max(1))),
            next_due: Instant::now(),
            frame_no: 0,
            buf: vec![0; width as usize * height as usize * 4],
            last_at: None,
        }
    }

    fn render(&mut self) {
        let w = self.width as usize;
        let h = self.height as usize;
        let t = self.frame_no as usize;
        let block = (w / 8).max(2);
        let bx = (t * 7) % w.saturating_sub(block).max(1);
        let by = h / 3;
        for (y, row) in self.buf.chunks_exact_mut(w * 4).enumerate() {
            let in_rows = y >= by && y < by + block;
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let in_block = in_rows && x >= bx && x < bx + block;
                if in_block {
                    px.copy_from_slice(&[255, 255, 255, 255]);
                } else {
                    let stripe = if ((x + 2 * t) / 16).is_multiple_of(2) {
                        24
                    } else {
                        0
                    };
                    px[0] = (((x + t * 3) / 4) & 0xff) as u8; // B
                    px[1] = (((y + t * 2) / 3) & 0xff) as u8; // G
                    px[2] = ((((x + y) / 6 + t) & 0xff) as u8).saturating_add(stripe); // R
                    px[3] = 255;
                }
            }
        }
    }
}

impl FrameSource for TestPattern {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>, CaptureError> {
        let now = Instant::now();
        if self.next_due > now {
            let wait = self.next_due - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next_due = self.next_due.max(now - self.interval) + self.interval;
        self.render();
        self.frame_no += 1;
        let at = Instant::now();
        self.last_at = Some(at);
        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            data: &self.buf,
            captured_at: at,
        }))
    }

    fn last_frame(&self) -> Option<Frame<'_>> {
        self.last_at.map(|at| Frame {
            width: self.width,
            height: self.height,
            data: &self.buf,
            captured_at: at,
        })
    }

    fn describe(&self) -> String {
        format!("test pattern {}x{}", self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_paced_even_frames() {
        let mut p = TestPattern::new(65, 33, 1000);
        assert_eq!(p.size(), (64, 32));
        assert!(p.last_frame().is_none());
        let a = p
            .next_frame(Duration::from_secs(1))
            .expect("frame")
            .expect("some")
            .data
            .to_vec();
        assert_eq!(a.len(), 64 * 32 * 4);
        let b = p
            .next_frame(Duration::from_secs(1))
            .expect("frame")
            .expect("some")
            .data
            .to_vec();
        assert_ne!(a, b, "frames must change over time");
        assert!(p.last_frame().is_some());
    }

    #[test]
    fn respects_timeout() {
        let mut p = TestPattern::new(16, 16, 1);
        p.next_frame(Duration::from_secs(1)).expect("first");
        let t = Instant::now();
        assert!(
            p.next_frame(Duration::from_millis(10))
                .expect("ok")
                .is_none()
        );
        assert!(t.elapsed() < Duration::from_millis(500));
    }
}

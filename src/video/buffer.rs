//! Bounded decoded pictures, retained for short movies and consumed as a queue for long ones.

use std::collections::VecDeque;
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::Arc;

use or_media::{MediaError, VideoStream};

use super::{sized, FRAME_BYTES};

const BUFFER_BYTES: usize = 128 * 1024 * 1024;
// Reserve four scaled pictures for the channel, conversion output and display.
const BUFFER_FRAMES: usize = BUFFER_BYTES / FRAME_BYTES - 4;
const AHEAD_SECONDS: f64 = 1.0;

type Picture = Arc<[u8]>;

pub(super) struct Decoder {
    pub(super) bytes: Arc<[u8]>,
    receiver: Option<Receiver<Result<Picture, MediaError>>>,
    pictures: VecDeque<Picture>,
    first: u64,
    total: u64,
    capacity: usize,
    retain: bool,
    ended: bool,
}

fn capacity(frames: u64, fps: f64, rate: f64) -> (usize, bool) {
    let retain = frames > 0 && frames <= BUFFER_FRAMES as u64;
    let capacity = if retain {
        frames as usize
    } else {
        reserve_capacity(fps, rate)
    };
    (capacity, retain)
}

fn reserve_capacity(fps: f64, rate: f64) -> usize {
    ((fps * rate * AHEAD_SECONDS).ceil() as usize).clamp(2, BUFFER_FRAMES)
}

impl Decoder {
    pub(super) fn set_rate(&mut self, fps: f64, rate: f64) {
        if !self.retain {
            self.capacity = reserve_capacity(fps, rate);
        }
    }

    pub(super) fn start(
        bytes: Arc<[u8]>,
        mut stream: Box<dyn VideoStream>,
        first: u64,
        total: u64,
        fps: f64,
        rate: f64,
    ) -> Self {
        let (capacity, retain) = capacity(total, fps, rate);
        let first = if retain { 0 } else { first };
        let (sender, receiver) = sync_channel(2);
        std::thread::spawn(move || {
            if first > 0 {
                if let Err(why) = stream.seek(first) {
                    let _ = sender.send(Err(why));
                    return;
                }
            }
            loop {
                let sent = match stream.next_frame() {
                    Ok(Some(frame)) => sender.send(Ok(sized(frame).into())).is_ok(),
                    Ok(None) => false,
                    Err(why) => {
                        let _ = sender.send(Err(why));
                        false
                    }
                };
                if !sent {
                    break;
                }
            }
        });
        Self {
            bytes,
            receiver: Some(receiver),
            pictures: VecDeque::new(),
            first,
            total,
            capacity,
            retain,
            ended: false,
        }
    }

    pub(super) fn pump(&mut self) -> Result<(), MediaError> {
        while self.pictures.len() < self.capacity {
            let Some(receiver) = &self.receiver else {
                break;
            };
            match receiver.try_recv() {
                Ok(Ok(frame)) => self.pictures.push_back(frame),
                Ok(Err(why)) => {
                    self.receiver = None;
                    self.ended = true;
                    return Err(why);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.receiver = None;
                    self.ended = true;
                    break;
                }
            }
        }
        if self.total > 0 && self.first + self.pictures.len() as u64 >= self.total {
            self.receiver = None;
            self.ended = true;
        }
        Ok(())
    }

    pub(super) fn last_frame(&self) -> Option<u64> {
        self.total.checked_sub(1)
    }

    pub(super) fn can_seek(&self, bytes: &Arc<[u8]>, at: u64) -> bool {
        Arc::ptr_eq(&self.bytes, bytes)
            && (self.retain || (self.first..self.first + self.pictures.len() as u64).contains(&at))
    }

    pub(super) fn frame(&mut self, at: u64) -> Option<Picture> {
        let offset = usize::try_from(at.checked_sub(self.first)?).ok()?;
        let frame = Arc::clone(self.pictures.get(offset)?);
        if !self.retain {
            self.pictures.drain(..=offset);
            self.first = at + 1;
        }
        Some(frame)
    }

    pub(super) fn finished(&self, at: u64) -> bool {
        self.ended && at >= self.first + self.pictures.len() as u64
    }

    pub(super) fn filling(&self) -> bool {
        self.receiver.is_some() && self.pictures.len() < self.capacity
    }

    pub(super) fn progress(&self, at: u64) -> f32 {
        if self.ended {
            return 1.0;
        }
        let (ready, target) = if self.retain {
            (self.pictures.len() as u64, self.total)
        } else {
            let ready = (self.first + self.pictures.len() as u64).saturating_sub(at);
            let wanted = self.capacity as u64;
            let target = if self.total > 0 {
                wanted.min(self.total.saturating_sub(at))
            } else {
                wanted
            };
            (ready, target)
        };
        if target == 0 {
            1.0
        } else {
            (ready as f32 / target as f32).min(1.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_movies_fit_whole_and_long_or_fast_movies_stay_bounded() {
        assert_eq!(capacity(115, 30.0, 1.0), (115, true));
        assert_eq!(
            capacity(900, 30.0, 1.0),
            (30, false),
            "one second at normal speed"
        );
        for (frames, fps) in [(900, 30.0), (u64::MAX, 240.0), (200, 1.0), (0, 30.0)] {
            let (count, retain) = capacity(frames, fps, 1.0);
            assert!(!retain);
            assert!(count >= 2);
            assert!((count + 4) * FRAME_BYTES <= BUFFER_BYTES);
        }
    }

    #[test]
    fn the_reserve_follows_the_selected_rate_without_throwing_away_decoded_frames() {
        for (rate, wanted) in [(0.25, 8), (0.5, 15), (1.0, 30), (2.0, 60), (4.0, 120)] {
            assert_eq!(capacity(900, 30.0, rate), (wanted, false));
            assert_eq!(capacity(115, 30.0, rate), (115, true));
        }
        let mut decoder = Decoder {
            bytes: Arc::from(&b"movie"[..]),
            receiver: None,
            pictures: (0..60).map(|v| Arc::from(vec![v])).collect(),
            first: 0,
            total: 900,
            capacity: 60,
            retain: false,
            ended: false,
        };
        decoder.set_rate(30.0, 0.5);
        assert_eq!(decoder.capacity, 15);
        assert_eq!(
            decoder.pictures.len(),
            60,
            "a rate change loses no prepared picture"
        );
        for at in 0..60 {
            assert_eq!(decoder.frame(at).as_deref(), Some(&[at as u8][..]));
        }
        decoder.set_rate(30.0, 2.0);
        assert_eq!(decoder.capacity, 60);
    }

    #[test]
    fn streaming_frames_are_consumed_but_cached_frames_survive_seeks() {
        for retain in [false, true] {
            let bytes: Arc<[u8]> = Arc::from(&b"movie"[..]);
            let mut decoder = Decoder {
                bytes: Arc::clone(&bytes),
                receiver: None,
                pictures: (0..3).map(|v| Arc::from(vec![v])).collect(),
                first: 0,
                total: 3,
                capacity: 3,
                retain,
                ended: true,
            };
            assert_eq!(decoder.frame(1).as_deref(), Some(&[1][..]));
            assert_eq!(decoder.can_seek(&bytes, 0), retain);
            assert_eq!(decoder.pictures.len(), if retain { 3 } else { 1 });
            assert!(!decoder.can_seek(&Arc::from(&b"other"[..]), 0));
            assert!(decoder.finished(3));
        }
    }
}

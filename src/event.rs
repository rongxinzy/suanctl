use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Tick,
    Resize(u16, u16),
}

pub struct EventHandler {
    tick_rate: Duration,
    last_tick: Instant,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        Self {
            tick_rate,
            last_tick: Instant::now(),
        }
    }

    pub fn next_event(&mut self) -> io::Result<Event> {
        loop {
            let elapsed = self.last_tick.elapsed();
            let timeout = self.tick_rate.saturating_sub(elapsed);
            if event::poll(timeout)? {
                match event::read()? {
                    CrosstermEvent::Key(key) => return Ok(Event::Key(key)),
                    CrosstermEvent::Resize(width, height) => {
                        return Ok(Event::Resize(width, height));
                    }
                    _ => continue,
                }
            }

            self.last_tick = Instant::now();
            return Ok(Event::Tick);
        }
    }
}

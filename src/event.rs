use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Tick,
    Resize(u16, u16),
}

/// 将鼠标事件映射为等价键盘事件：滚轮上下 = 上下滚动。
/// 其余鼠标事件（移动/点击）不消费，保持界面简洁。
pub fn map_mouse_event(mouse: MouseEvent) -> Option<Event> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            Some(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
        }
        MouseEventKind::ScrollDown => {
            Some(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
        }
        _ => None,
    }
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
                    CrosstermEvent::Mouse(mouse) => {
                        if let Some(mapped) = map_mouse_event(mouse) {
                            return Ok(mapped);
                        }
                    }
                    _ => continue,
                }
            }

            self.last_tick = Instant::now();
            return Ok(Event::Tick);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::MouseEventKind;

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn scroll_wheel_maps_to_arrow_keys() {
        assert_eq!(
            map_mouse_event(mouse(MouseEventKind::ScrollUp)),
            Some(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
        );
        assert_eq!(
            map_mouse_event(mouse(MouseEventKind::ScrollDown)),
            Some(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
        );
    }

    #[test]
    fn non_scroll_mouse_events_are_ignored() {
        assert_eq!(map_mouse_event(mouse(MouseEventKind::Moved)), None);
        assert_eq!(
            map_mouse_event(mouse(MouseEventKind::Down(
                crossterm::event::MouseButton::Left
            ))),
            None
        );
        assert_eq!(
            map_mouse_event(mouse(MouseEventKind::Up(
                crossterm::event::MouseButton::Left
            ))),
            None
        );
    }
}

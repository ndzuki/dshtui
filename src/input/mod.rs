//! 输入层：把 crossterm 按键转换为 AppState 可消费的 Command。

pub mod keymap;

pub use keymap::{map_key, Command, InputMode, KeyDecoder, Keymap};

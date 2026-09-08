//! focusd 库入口。
//!
//! 拆出 lib 是为了集成测试（tests/*.rs）能直接复用 D-Bus 服务组装、
//! 契约接口与 Dedup，而不必通过 CLI 黑盒验证。

pub mod backend;
pub mod dbus;

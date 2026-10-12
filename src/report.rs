use chrono::Local;

/// 同步过程中的日志与进度回调。
///
/// 后台搬运轮次和手动同步都只通过它输出，便于 UI、CLI 与测试各自实现。
pub trait Reporter: Send + Sync {
    fn log(&self, text: &str);
    fn progress(&self, value: u8);
}

/// 丢弃全部输出的实现，供测试使用。
#[cfg(test)]
pub struct NullReporter;

#[cfg(test)]
impl Reporter for NullReporter {
    fn log(&self, _text: &str) {}
    fn progress(&self, _value: u8) {}
}

/// 给日志行加上 `YYYYMMDDHHmmss` 前缀。
pub fn timestamped(text: &str) -> String {
    format!("{} {}", Local::now().format("%Y%m%d%H%M%S"), text)
}

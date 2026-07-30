pub trait Capability: Send + Sync {
    fn name(&self) -> &str;
}

pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn origin() -> Self {
        Self { x: 0, y: 0 }
    }
}

impl Default for Point {
    fn default() -> Self {
        Self::origin()
    }
}

impl Drop for Point {
    fn drop(&mut self) {}
}

unsafe impl Send for Point {}

pub(super) struct SurfaceFrame {
    frame: Option<wgpu::SurfaceTexture>,
    texture: TextureCleanup,
}

impl SurfaceFrame {
    pub(super) fn new(frame: wgpu::SurfaceTexture) -> Self {
        Self {
            texture: TextureCleanup(frame.texture.clone()),
            frame: Some(frame),
        }
    }

    pub(super) fn texture(&self) -> &wgpu::Texture {
        &self.texture.0
    }

    pub(super) fn present(mut self) {
        if let Some(frame) = self.frame.take() {
            frame.present();
        }
    }
}

struct TextureCleanup(wgpu::Texture);

impl Drop for TextureCleanup {
    fn drop(&mut self) {
        // wgpu 29 can retain an acquired texture after device loss. Release its raw
        // resources after present/discard, before the owning surface is destroyed.
        // https://github.com/gfx-rs/wgpu/issues/9277
        self.0.destroy();
    }
}

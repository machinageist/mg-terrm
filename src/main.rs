use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use winit::{
    event::{ElementState, Event, WindowEvent},
    event_loop::EventLoop,
    keyboard::{Key, NamedKey},
    window::WindowBuilder,
};

#[derive(Clone, Debug)]
struct Cell {
    c: char,
}

struct TerminalGrid {
    cols: usize,
    rows: usize,
    cursor_x: usize,
    cursor_y: usize,
    cells: Vec<Vec<Cell>>,
    dirty: bool,
}

impl TerminalGrid {
    fn new(cols: usize, rows: usize) -> Self {
        let default_cell = Cell { c: ' ' };

        Self {
            cols,
            rows,
            cursor_x: 0,
            cursor_y: 0,
            cells: vec![vec![default_cell; cols]; rows],
            dirty: true,
        }
    }

    fn put_char(&mut self, c: char) {
        match c {
            // Carriage return: move to beginning of current line.
            '\r' => {
                self.cursor_x = 0;
            }

            // Line feed: move down one row.
            '\n' => {
                self.cursor_y += 1;
                self.ensure_cursor_visible();
            }

            // ASCII backspace.
            //
            // A typical terminal line discipline echoes Backspace as:
            //
            //     \b \b
            //
            // The first \b moves the cursor left, the space overwrites
            // the old character, and the second \b moves the cursor left
            // again.
            '\x08' => {
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                }
            }

            // Ignore other ASCII control characters for now.
            c if c.is_control() => {}

            // Printable character.
            _ => {
                self.ensure_cursor_visible();

                self.cells[self.cursor_y][self.cursor_x].c = c;
                self.cursor_x += 1;

                // Wrap to the next line when reaching the right edge.
                if self.cursor_x >= self.cols {
                    self.cursor_x = 0;
                    self.cursor_y += 1;
                    self.ensure_cursor_visible();
                }
            }
        }

        self.dirty = true;
    }

    fn ensure_cursor_visible(&mut self) {
        if self.cursor_y < self.rows {
            return;
        }

        while self.cursor_y >= self.rows {
            self.cells.remove(0);

            let default_cell = Cell { c: ' ' };
            self.cells.push(vec![default_cell; self.cols]);

            self.cursor_y -= 1;
        }
    }
}

fn spawn_terminal_shell() -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to create PTY");

    let shell_path = "/home/mgeist/geistos/mg-suite/mg-shellr/target/release/mg-shellr";

    let cmd = CommandBuilder::new(shell_path);

    pair.slave
        .spawn_command(cmd)
        .expect("failed to spawn mg-shellr");

    let reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone PTY reader");

    let writer = pair
        .master
        .take_writer()
        .expect("failed to acquire PTY writer");

    (reader, writer)
}

fn handle_key_event(key_event: &winit::event::KeyEvent, pty_writer: &mut Box<dyn Write + Send>) {
    if key_event.state != ElementState::Pressed {
        return;
    }

    let bytes: Option<Vec<u8>> = match &key_event.logical_key {
        // Printable/text input.
        Key::Character(text) => Some(text.as_bytes().to_vec()),

        // Terminal control keys.
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(b"\x7f".to_vec()),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),

        // ANSI cursor movement sequences.
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),

        _ => None,
    };

    if let Some(bytes) = bytes {
        if pty_writer.write_all(&bytes).is_ok() {
            let _ = pty_writer.flush();
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().expect("failed to create event loop");

    let window = Arc::new(
        WindowBuilder::new()
            .with_title("mg-terrm")
            .with_inner_size(winit::dpi::PhysicalSize::new(800, 600))
            .build(&event_loop)
            .expect("failed to create window"),
    );

    // ---------------------------------------------------------------------
    // WGPU
    // ---------------------------------------------------------------------

    let instance = wgpu::Instance::default();

    let surface = instance
        .create_surface(window.clone())
        .expect("failed to create surface");

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: Some(&surface),
    }))
    .expect("failed to find GPU adapter");

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("Terminal Device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
        },
        None,
    ))
    .expect("failed to create GPU device");

    let size = window.inner_size();

    let mut config = surface
        .get_default_config(&adapter, size.width, size.height)
        .expect("failed to create surface configuration");

    surface.configure(&device, &config);

    // ---------------------------------------------------------------------
    // Glyphon
    // ---------------------------------------------------------------------

    let mut font_system = glyphon::FontSystem::new();
    let mut swash_cache = glyphon::SwashCache::new();

    let cache = glyphon::Cache::new(&device);

    let mut atlas = glyphon::TextAtlas::new(&device, &queue, &cache, config.format);

    let mut text_renderer =
        glyphon::TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

    let mut viewport = glyphon::Viewport::new(&device, &cache);

    viewport.update(
        &queue,
        glyphon::Resolution {
            width: size.width,
            height: size.height,
        },
    );

    let mut buffer = glyphon::Buffer::new(&mut font_system, glyphon::Metrics::new(16.0, 20.0));

    buffer.set_size(
        &mut font_system,
        Some(size.width as f32),
        Some(size.height as f32),
    );

    // ---------------------------------------------------------------------
    // Terminal state
    // ---------------------------------------------------------------------

    let grid = Arc::new(Mutex::new(TerminalGrid::new(80, 24)));

    let (mut pty_reader, mut pty_writer) = spawn_terminal_shell();

    // ---------------------------------------------------------------------
    // PTY reader thread
    // ---------------------------------------------------------------------

    let reader_grid = Arc::clone(&grid);
    let window_handle = Arc::clone(&window);

    thread::spawn(move || {
        let mut buf = [0u8; 4096];

        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break,

                Ok(n) => {
                    if let Ok(mut locked_grid) = reader_grid.lock() {
                        let text = String::from_utf8_lossy(&buf[..n]);

                        for c in text.chars() {
                            locked_grid.put_char(c);
                        }
                    }

                    window_handle.request_redraw();
                }

                Err(_) => break,
            }
        }
    });

    // ---------------------------------------------------------------------
    // Event loop
    // ---------------------------------------------------------------------

    let _ = event_loop.run(move |event, elwt| match event {
        Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => {
            elwt.exit();
        }

        Event::WindowEvent {
            event: WindowEvent::Resized(new_size),
            ..
        } => {
            if new_size.width == 0 || new_size.height == 0 {
                return;
            }

            config.width = new_size.width;
            config.height = new_size.height;

            surface.configure(&device, &config);

            viewport.update(
                &queue,
                glyphon::Resolution {
                    width: new_size.width,
                    height: new_size.height,
                },
            );

            buffer.set_size(
                &mut font_system,
                Some(new_size.width as f32),
                Some(new_size.height as f32),
            );

            window.request_redraw();
        }

        Event::WindowEvent {
            event: WindowEvent::KeyboardInput {
                event: key_event, ..
            },
            ..
        } => {
            handle_key_event(&key_event, &mut pty_writer);
        }

        Event::WindowEvent {
            event: WindowEvent::RedrawRequested,
            ..
        } => {
            let mut should_render = false;
            let mut text_content = String::new();

            if let Ok(mut locked_grid) = grid.lock() {
                if locked_grid.dirty {
                    for row in &locked_grid.cells {
                        for cell in row {
                            text_content.push(cell.c);
                        }

                        text_content.push('\n');
                    }

                    locked_grid.dirty = false;
                    should_render = true;
                }
            }

            if !should_render {
                return;
            }

            buffer.set_text(
                &mut font_system,
                &text_content,
                glyphon::Attrs::new().family(glyphon::Family::Monospace),
                glyphon::Shaping::Basic,
            );

            let text_area = glyphon::TextArea {
                buffer: &buffer,
                left: 10.0,
                top: 10.0,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 0,
                    top: 0,
                    right: window.inner_size().width as i32,
                    bottom: window.inner_size().height as i32,
                },
                default_color: glyphon::Color::rgb(255, 255, 255),
                custom_glyphs: &[],
            };

            text_renderer
                .prepare(
                    &device,
                    &queue,
                    &mut font_system,
                    &mut atlas,
                    &viewport,
                    [text_area],
                    &mut swash_cache,
                )
                .expect("failed to prepare text renderer");

            let frame = match surface.get_current_texture() {
                Ok(frame) => frame,

                Err(wgpu::SurfaceError::Lost) => {
                    surface.configure(&device, &config);
                    window.request_redraw();
                    return;
                }

                Err(wgpu::SurfaceError::OutOfMemory) => {
                    elwt.exit();
                    return;
                }

                Err(_) => {
                    window.request_redraw();
                    return;
                }
            };

            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Render Pass"),

                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,

                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.05,
                                g: 0.05,
                                b: 0.07,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],

                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

                text_renderer
                    .render(&atlas, &viewport, &mut pass)
                    .expect("failed to render terminal text");
            }

            queue.submit(std::iter::once(encoder.finish()));

            frame.present();

            atlas.trim();
        }

        _ => {}
    });
}

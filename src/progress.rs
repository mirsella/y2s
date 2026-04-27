use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

#[derive(Clone)]
pub struct Progress {
    multi: MultiProgress,
    phase: ProgressBar,
    rendering_paused: Arc<AtomicBool>,
}

pub struct ProgressRenderPause(Progress);

impl Progress {
    pub fn new() -> Self {
        let multi = MultiProgress::new();
        let phase = multi.add(ProgressBar::new_spinner());
        phase.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .expect("phase progress template is valid"),
        );
        phase.enable_steady_tick(std::time::Duration::from_millis(120));
        Self {
            multi,
            phase,
            rendering_paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_phase(&self, message: impl Into<String>) {
        self.phase.set_message(message.into());
    }

    pub fn track_bar(&self, len: usize, message: impl Into<String>) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(len as u64));
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {msg} [{elapsed_precise}] [{bar:32.cyan/blue}] {pos}/{len} ETA {eta}",
            )
            .expect("track progress template is valid")
            .progress_chars("=>-"),
        );
        bar.set_message(message.into());
        bar
    }

    pub fn spinner(&self, message: impl Into<String>) -> ProgressBar {
        let spinner = self.multi.add(ProgressBar::new_spinner());
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .expect("spinner progress template is valid"),
        );
        spinner.enable_steady_tick(std::time::Duration::from_millis(120));
        spinner.set_message(message.into());
        spinner
    }

    pub fn rendering_paused(&self) -> bool {
        self.rendering_paused.load(Ordering::Relaxed)
    }

    pub fn println(&self, message: impl AsRef<str>) {
        if self.rendering_paused() || self.multi.println(message.as_ref()).is_err() {
            eprintln!("{}", message.as_ref());
        }
    }

    pub fn pause_rendering(&self) -> ProgressRenderPause {
        self.rendering_paused.store(true, Ordering::Relaxed);
        self.phase.disable_steady_tick();
        let _ = self.multi.clear();
        self.multi.set_draw_target(ProgressDrawTarget::hidden());
        ProgressRenderPause(self.clone())
    }

    pub fn finish(&self) {
        self.phase.finish_and_clear();
        let _ = self.multi.clear();
    }
}

impl Drop for ProgressRenderPause {
    fn drop(&mut self) {
        self.0.multi.set_draw_target(ProgressDrawTarget::stderr());
        self.0.rendering_paused.store(false, Ordering::Relaxed);
        self.0
            .phase
            .enable_steady_tick(std::time::Duration::from_millis(120));
        let _ = self.0.multi.clear();
    }
}

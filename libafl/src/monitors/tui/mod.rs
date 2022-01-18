//! Monitor based on tui-rs

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use hashbrown::HashMap;
use num_traits::PrimInt;
use std::{error::Error, io, io::BufRead, marker::Sync, time::Instant};
use tui::{
    backend::{Backend, CrosstermBackend},
    Terminal,
};

use std::{
    cell::RefCell,
    cmp::{max, min},
    io::Stdout,
    string::String,
    sync::{Arc, RwLock},
    thread,
    time::Duration,
    vec::Vec,
};

#[cfg(feature = "introspection")]
use super::{ClientPerfMonitor, PerfFeature};

use crate::{
    bolts::{current_time, format_duration_hms},
    monitors::{ClientStats, Monitor, UserStats},
};

mod ui;
use ui::TuiUI;

#[derive(Copy, Clone)]
pub struct TimedStat {
    pub time: Duration,
    pub item: u64,
}

impl Into<(f64, f64)> for TimedStat {
    fn into(self) -> (f64, f64) {
        ((self.time.as_secs()) as f64, self.item as f64)
    }
}

#[derive(Clone)]
pub struct TimedStats {
    pub series: Vec<TimedStat>,
    pub max: u64,
    pub min: u64,
}

impl TimedStats {
    pub fn new() -> Self {
        Self {
            series: vec![],
            max: u64::MIN,
            min: u64::MAX,
        }
    }

    pub fn add(&mut self, time: Duration, item: u64) {
        if self.series.is_empty() || self.series[self.series.len() - 1].item != item {
            self.series.push(TimedStat { time, item });
            self.max = max(self.max, item);
            self.min = min(self.min, item);
        }
    }

    pub fn add_now(&mut self, item: u64) {
        if self.series.is_empty() || self.series[self.series.len() - 1].item != item {
            self.series.push(TimedStat {
                time: current_time(),
                item,
            });
            self.max = max(self.max, item);
            self.min = min(self.min, item);
        }
    }
}

#[cfg(feature = "introspection")]
#[derive(Default, Clone)]
pub struct PerfTuiContext {
    pub scheduler: f64,
    pub manager: f64,
    pub unmeasured: f64,
    pub stages: Vec<Vec<(String, f64)>>,
    pub feedbacks: Vec<(String, f64)>,
}

#[cfg(feature = "introspection")]
impl PerfTuiContext {
    pub fn grab_data(&mut self, m: &ClientPerfMonitor) {
        // Calculate the elapsed time from the monitor
        let elapsed: f64 = m.elapsed_cycles() as f64;

        // Calculate the percentages for each benchmark
        self.scheduler = m.scheduler_cycles() as f64 / elapsed;
        self.manager = m.manager_cycles() as f64 / elapsed;

        // Calculate the remaining percentage that has not been benchmarked
        let mut other_percent = 1.0;
        other_percent -= self.scheduler;
        other_percent -= self.manager;

        self.stages.clear();

        // Calculate each stage
        // Make sure we only iterate over used stages
        for (stage_index, features) in m.used_stages() {
            let mut features_percentages = vec![];

            for (feature_index, feature) in features.iter().enumerate() {
                // Calculate this current stage's percentage
                let feature_percent = *feature as f64 / elapsed;

                // Ignore this feature if it isn't used
                if feature_percent == 0.0 {
                    continue;
                }

                // Update the other percent by removing this current percent
                other_percent -= feature_percent;

                // Get the actual feature from the feature index for printing its name
                let feature: PerfFeature = feature_index.into();
                features_percentages.push((format!("{:?}", feature), feature_percent));
            }

            self.stages.push(features_percentages);
        }

        self.feedbacks.clear();

        for (feedback_name, feedback_time) in m.feedbacks() {
            // Calculate this current stage's percentage
            let feedback_percent = *feedback_time as f64 / elapsed;

            // Ignore this feedback if it isn't used
            if feedback_percent == 0.0 {
                continue;
            }

            // Update the other percent by removing this current percent
            other_percent -= feedback_percent;

            self.feedbacks
                .push((feedback_name.clone(), feedback_percent));
        }

        self.unmeasured = other_percent;
    }
}

#[derive(Default, Clone)]
pub struct ClientTuiContext {
    pub corpus: u64,
    pub objectives: u64,
    pub executions: u64,
    pub exec_sec: u64,

    pub user_stats: HashMap<String, UserStats>,
}

impl ClientTuiContext {
    pub fn grab_data(&mut self, client: &ClientStats, exec_sec: u64) {
        self.corpus = client.corpus_size;
        self.objectives = client.objective_size;
        self.executions = client.executions;
        self.exec_sec = exec_sec;

        for (key, val) in &client.user_monitor {
            self.user_stats.insert(key.clone(), val.clone());
        }
    }
}

#[derive(Clone)]
pub struct TuiContext {
    pub graphs: Vec<String>,

    pub corpus_size_timed: TimedStats,
    pub objective_size_timed: TimedStats,
    pub execs_per_sec_timed: TimedStats,

    #[cfg(feature = "introspection")]
    pub introspection: HashMap<usize, PerfTuiContext>,

    pub clients: HashMap<usize, ClientTuiContext>,

    pub client_logs: Vec<String>, // TODO set max size

    pub clients_num: usize,
    pub total_execs: u64,
    pub start_time: Duration,
}

impl TuiContext {
    pub fn new(start_time: Duration) -> Self {
        Self {
            graphs: vec!["corpus".into(), "objectives".into(), "exec/sec".into()],
            corpus_size_timed: TimedStats::new(),
            objective_size_timed: TimedStats::new(),
            execs_per_sec_timed: TimedStats::new(),

            #[cfg(feature = "introspection")]
            introspection: HashMap::default(),
            clients: HashMap::default(),

            client_logs: vec![],

            clients_num: 0,
            total_execs: 0,
            start_time,
        }
    }
}

/// Tracking monitor during fuzzing and display with tui-rs.
#[derive(Clone)]
pub struct TuiMonitor {
    pub context: Arc<RwLock<TuiContext>>,

    start_time: Duration,
    client_stats: Vec<ClientStats>,
}

impl Monitor for TuiMonitor {
    /// the client monitor, mutable
    fn client_stats_mut(&mut self) -> &mut Vec<ClientStats> {
        &mut self.client_stats
    }

    /// the client monitor
    fn client_stats(&self) -> &[ClientStats] {
        &self.client_stats
    }

    /// Time this fuzzing run stated
    fn start_time(&mut self) -> Duration {
        self.start_time
    }

    fn display(&mut self, event_msg: String, sender_id: u32) {
        let cur_time = current_time();

        {
            let execsec = self.execs_per_sec();
            let totalexec = self.total_execs();
            let run_time = cur_time - self.start_time;

            let mut ctx = self.context.write().unwrap();
            ctx.corpus_size_timed.add(run_time, self.corpus_size());
            ctx.objective_size_timed
                .add(run_time, self.objective_size());
            ctx.execs_per_sec_timed.add(run_time, execsec);
            ctx.total_execs = totalexec;
            ctx.clients_num = self.client_stats.len();
        }

        let client = self.client_stats_mut_for(sender_id);
        let exec_sec = client.execs_per_sec(cur_time);

        let sender = format!("#{}", sender_id);
        let pad = if event_msg.len() + sender.len() < 13 {
            " ".repeat(13 - event_msg.len() - sender.len())
        } else {
            String::new()
        };
        let head = format!("{}{} {}", event_msg, pad, sender);
        let mut fmt = format!(
            "[{}] corpus: {}, objectives: {}, executions: {}, exec/sec: {}",
            head, client.corpus_size, client.objective_size, client.executions, exec_sec
        );
        for (key, val) in &client.user_monitor {
            fmt += &format!(", {}: {}", key, val);
        }

        {
            let client = &self.client_stats()[sender_id as usize];
            let mut ctx = self.context.write().unwrap();
            ctx.clients
                .entry(sender_id as usize)
                .or_default()
                .grab_data(client, exec_sec);
            ctx.client_logs.push(fmt);
        }

        #[cfg(feature = "introspection")]
        {
            // Print the client performance monitor. Skip the Client 0 which is the broker
            for (i, client) in self.client_stats.iter().skip(1).enumerate() {
                self.context
                    .write()
                    .unwrap()
                    .introspection
                    .entry(i)
                    .or_default()
                    .grab_data(&client.introspection_monitor);
            }
        }
    }
}

impl TuiMonitor {
    /// Creates the monitor
    pub fn new(title: String, enhanced_graphics: bool) -> Self {
        Self::with_time(title, enhanced_graphics, current_time())
    }

    /// Creates the monitor with a given `start_time`.
    pub fn with_time(title: String, enhanced_graphics: bool, start_time: Duration) -> Self {
        let context = Arc::new(RwLock::new(TuiContext::new(start_time)));
        run_tui_thread(
            context.clone(),
            Duration::from_millis(250),
            title,
            enhanced_graphics,
        );
        Self {
            context,
            start_time,
            client_stats: vec![],
        }
    }
}

fn run_tui_thread(
    context: Arc<RwLock<TuiContext>>,
    tick_rate: Duration,
    title: String,
    enhanced_graphics: bool,
) {
    thread::spawn(move || -> io::Result<()> {
        // setup terminal
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut ui = TuiUI::new(title, enhanced_graphics);

        let mut last_tick = Instant::now();
        loop {
            terminal.draw(|f| ui.draw(f, &context))?;

            let timeout = tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));
            if crossterm::event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char(c) => ui.on_key(c),
                        KeyCode::Left => ui.on_left(),
                        //KeyCode::Up => ui.on_up(),
                        KeyCode::Right => ui.on_right(),
                        //KeyCode::Down => ui.on_down(),
                        _ => {}
                    }
                }
            }
            if last_tick.elapsed() >= tick_rate {
                //context.on_tick();
                last_tick = Instant::now();
            }
            if ui.should_quit {
                // restore terminal
                disable_raw_mode()?;
                execute!(
                    terminal.backend_mut(),
                    LeaveAlternateScreen,
                    DisableMouseCapture
                )?;
                terminal.show_cursor()?;

                println!("\nPress Control-C to stop the fuzzers, otherwise press Enter to resume the visualization\n");

                let mut line = String::new();
                io::stdin().lock().read_line(&mut line)?;

                // setup terminal
                let mut stdout = io::stdout();
                enable_raw_mode()?;
                execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

                ui.should_quit = false;
            }
        }

        Ok(())
    });
}

//! Durable work: schedules, the session goal and shell jobs as durable tasks that survive worker restarts. This is
//! the one entry point a composition root calls.
//!
//! On a worker's open, after its task host (the task journal's owner) and its host with a jobs journal exist:
//!
//! ```text
//! let host = Arc::new(Host::new(cwd, jobs_dir, spill_dir, guard)            // shell jobs: recovered here,
//!     .with_journal(Arc::new(FileJobJournal::new(jobs_journal)))?);         //   from their processes
//! let schedules = Arc::new(SchedulesModule::new(store("schedules"), None));
//! let goal = Arc::new(GoalDriver::new(store("goal"),
//!     Arc::new(TaskGoalJobs::new(tasks.clone(), host.clone())), clock));
//! let apis = KernelApis::new(inner).with_host(tasks.clone(), lane)
//!     .with(schedules.clone()).with(goal.clone());                           // schedules.*, goals.*, goal.*
//! let work = DurableWork::open(DurableWorkOptions::new(tasks.clone())
//!     .schedules(schedules).goal(goal).jobs(host))?;                         // recover + start the tick
//! ...
//! work.close();      // session close: stop the tick, stop starting goal jobs, stop shell jobs
//! work.hand_over();  // a successor worker takes over: the same, but shell jobs keep running for it
//! ```
//!
//! [`DurableWork::open`] reconciles schedules with the task journal (goal links of admitted firings), reconciles the
//! goal (adopts an admitted run, restarts an interrupted one, settles one that ended), and starts the
//! [`ScheduleTicker`], which fires due slots on its own, including those missed while no worker ran (coalesced into
//! one run). The task end observer still calls [`GoalDriver::task_ended`], and `/goal` still goes to
//! [`GoalDriver::command`].

use crate::goal::GoalDriver;
use crate::schedule_tick::{ScheduleTicker, SystemTickClock, TickClock, TickObserver};
use crate::schedules::SchedulesModule;
use crate::tasks::TaskHost;
use crate::util::lock;
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

/// What [`DurableWork::open`] takes. Every part is optional except the task host.
pub struct DurableWorkOptions {
    /// The task journal's owner: firings and goal runs are admitted through it.
    pub tasks: Arc<dyn TaskHost>,
    pub schedules: Option<Arc<SchedulesModule>>,
    pub goal: Option<Arc<GoalDriver>>,
    /// The host whose shell jobs this worker owns (recovered by `Host::with_journal`); stopped or handed over on
    /// close.
    pub jobs: Option<Arc<host::Host>>,
    /// The tick's clock (default: wall time).
    pub clock: Arc<dyn TickClock>,
    /// Sees every tick's report (`{fired, failed}`), e.g. to wake the session when a firing was admitted.
    pub on_tick: Option<TickObserver>,
}

impl DurableWorkOptions {
    pub fn new(tasks: Arc<dyn TaskHost>) -> DurableWorkOptions {
        DurableWorkOptions {
            tasks,
            schedules: None,
            goal: None,
            jobs: None,
            clock: Arc::new(SystemTickClock),
            on_tick: None,
        }
    }

    pub fn schedules(mut self, schedules: Arc<SchedulesModule>) -> Self {
        self.schedules = Some(schedules);
        self
    }

    pub fn goal(mut self, goal: Arc<GoalDriver>) -> Self {
        self.goal = Some(goal);
        self
    }

    pub fn jobs(mut self, jobs: Arc<host::Host>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    pub fn clock(mut self, clock: Arc<dyn TickClock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn on_tick(mut self, observer: TickObserver) -> Self {
        self.on_tick = Some(observer);
        self
    }
}

/// What recovery found on open.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovered {
    /// The session goal after reconciliation (`None`: no goal).
    pub goal: Option<Value>,
    /// The earliest time a schedule fires (`None`: none will).
    pub next_fire: Option<i64>,
    /// The shell jobs as `rlm.jobs()` lists them, newest first.
    pub jobs: Vec<Value>,
}

pub struct DurableWork {
    ticker: Mutex<Option<ScheduleTicker>>,
    goal: Option<Arc<GoalDriver>>,
    jobs: Option<Arc<host::Host>>,
    recovered: Recovered,
}

impl DurableWork {
    /// Recovers schedules, the goal and the jobs list, then starts the schedule tick.
    pub fn open(options: DurableWorkOptions) -> Result<DurableWork, String> {
        let mut next_fire = None;
        if let Some(schedules) = &options.schedules {
            schedules.start(options.tasks.as_ref())?;
            next_fire = schedules.next_fire()?;
        }
        let goal = match &options.goal {
            Some(driver) => driver.recover()?,
            None => None,
        };
        let jobs = match &options.jobs {
            Some(host) => host
                .handle("shell.list", Map::new(), &Signal::new())?
                .as_array()
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let ticker = options
            .schedules
            .map(|schedules| ScheduleTicker::start(schedules, options.tasks, options.clock, options.on_tick));
        Ok(DurableWork {
            ticker: Mutex::new(ticker),
            goal: options.goal,
            jobs: options.jobs,
            recovered: Recovered { goal, next_fire, jobs },
        })
    }

    pub fn recovered(&self) -> &Recovered {
        &self.recovered
    }

    /// Re-reads the next fire time now (after the clock moved or the schedules document changed elsewhere).
    pub fn wake(&self) {
        if let Some(ticker) = lock(&self.ticker).as_ref() {
            ticker.wake();
        }
    }

    fn stop(&self) {
        let ticker = lock(&self.ticker).take();
        if let Some(ticker) = ticker {
            ticker.close();
        }
        if let Some(goal) = &self.goal {
            goal.close();
        }
    }

    /// Session close: stops the tick (a tick in progress finishes), stops starting goal jobs, and stops every
    /// running shell job. Durable state stays for the next open. Idempotent.
    pub fn close(&self) {
        self.stop();
        if let Some(jobs) = &self.jobs {
            jobs.stop_jobs();
        }
    }

    /// A successor worker takes over the session: as [`DurableWork::close`], but running shell jobs keep running
    /// and are left journaled as running for the successor to follow.
    pub fn hand_over(&self) {
        self.stop();
        if let Some(jobs) = &self.jobs {
            jobs.release_jobs();
        }
    }
}

impl Drop for DurableWork {
    fn drop(&mut self) {
        self.stop();
    }
}

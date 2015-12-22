use std::iter::FromIterator;
use std::collections::VecDeque;
use std::thread;
use std::sync::mpsc;
use std::sync::mpsc::{Sender, Receiver};
use std::sync::{Arc, RwLock, Mutex, MutexGuard, RwLockReadGuard};
use comm;
use utils::{WorkerID, ObjRef, ObjTable, FnTable};
use server::Worker;
use protobuf::RepeatedField;

/// Notify the scheduler that something happened
pub enum Event {
  /// A worker becomes available for computation.
  Worker(WorkerID),
  /// An object has been returned by a worker.
  Obj(ObjRef),
  /// A job is being scheduled.
  Job(comm::Call),
  /// A pull request was issued.
  Pull(WorkerID, ObjRef),
  /// A new worker has been added.
  Register(WorkerID, Sender<comm::Message>),
  /// Dump status of the scheduler.
  Debug(WorkerID)
}

/// A scheduler assigns incoming jobs to workers. It communicates with the worker pool through
/// channels. If a job is scheduled or a worker becomes available, this is signaled to the
/// Scheduler using the channel returned by the `Scheduler::start` method. The scheduler signals the
/// execution of a function call to the appropriate worker thread via a channel that is registered
/// using `Event::Register`.
pub struct Scheduler {
  objtable: Arc<Mutex<ObjTable>>,
  fntable: Arc<RwLock<FnTable>>,
}

impl Scheduler {
  /// Start the scheduling thread.
  pub fn start(objtable: Arc<Mutex<ObjTable>>, fntable: Arc<RwLock<FnTable>>) -> Sender<Event> {
    let (event_sender, event_receiver) = mpsc::channel(); // notify the scheduler that a worker, job or object becomes available
    let scheduler = Scheduler { objtable: objtable.clone(), fntable: fntable.clone() };
    scheduler.start_dispatch_thread(event_receiver);
    return event_sender
  }

  fn send_function_call(workers: &Vec<Sender<comm::Message>>, workerid: WorkerID, job: comm::Call) {
    let mut msg = comm::Message::new();
    msg.set_field_type(comm::MessageType::INVOKE);
    msg.set_call(job);
    workers[workerid].send(msg).unwrap();
  }

  fn send_pull_request(workers: &Vec<Sender<comm::Message>>, workerid: WorkerID, objref: ObjRef) {
    let mut msg = comm::Message::new();
    msg.set_field_type(comm::MessageType::PULL);
    msg.set_workerid(workerid as u64);
    msg.set_objref(objref);
    workers[workerid].send(msg).unwrap();
  }

  fn send_debugging_info(socket: &Sender<comm::Message>, worker_queue: &VecDeque<WorkerID>, job_queue: &VecDeque<comm::Call>) {
    let mut scheduler_info = comm::SchedulerInfo::new();
    scheduler_info.set_worker_queue(worker_queue.iter().map(|x| *x as u64).collect());
	let mut jobs = Vec::new();
	for job in job_queue.iter() {
		jobs.push(job.clone());
	}
	scheduler_info.set_job_queue(RepeatedField::from_vec(jobs));
    let mut msg = comm::Message::new();
    msg.set_field_type(comm::MessageType::DEBUG);
	msg.set_scheduler_info(scheduler_info);
    socket.send(msg).unwrap();
  }

  /// Find job whose dependencies are met.
  fn find_next_job(self: &Scheduler, workerid: WorkerID, job_queue: &VecDeque<comm::Call>) -> Option<usize> {
    let objtable = &self.objtable.lock().unwrap();
    for (i, job) in job_queue.iter().enumerate() {
      if self.fntable.read().unwrap()[job.get_name()].binary_search(&workerid).is_ok() && self.can_run(job, objtable) {
        return Some(i);
      }
    }
    return None;
  }

  fn can_run(self: &Scheduler, job: &comm::Call, objtable: &MutexGuard<ObjTable>) -> bool {
    for objref in job.get_args() {
      if objtable[*objref as usize].len() == 0 {
        return false;
      }
    }
    return true;
  }

  // TODO: replace fntable vector with bitfield
  fn find_next_worker(self: &Scheduler, job: &comm::Call, worker_queue: &VecDeque<usize>) -> Option<usize> {
    let objtable = &self.objtable.lock().unwrap();
    for (i, workerid) in worker_queue.iter().enumerate() {
      if self.fntable.read().unwrap()[job.get_name()].binary_search(workerid).is_ok() && self.can_run(job, objtable) {
        return Some(i);
      }
    }
    return None;
  }

  // will be notified of workers or jobs that become available throught the worker_notify or job_notify channel
  fn start_dispatch_thread(self: Scheduler, event_notify: Receiver<Event>) {
    thread::spawn(move || {
      let mut workers = Vec::<Sender<comm::Message>>::new();
      let mut worker_queue = VecDeque::<WorkerID>::new();
      let mut job_queue = VecDeque::<comm::Call>::new();
      let mut pull_queue = VecDeque::<(WorkerID, ObjRef)>::new();

      loop {
        // use the most simple algorithms for now
        match event_notify.recv().unwrap() {
          Event::Worker(workerid) => {
            match self.find_next_job(workerid, &job_queue) {
              Some(jobidx) => {
                let job = job_queue.swap_front_remove(jobidx).unwrap();
                Scheduler::send_function_call(&mut workers, workerid, job);
              }
              None => {
                worker_queue.push_back(workerid);
              }
            }
          },
          Event::Job(job) => {
            match self.find_next_worker(&job, &worker_queue) {
              Some(workeridx) => {
                let workerid = worker_queue.swap_front_remove(workeridx).unwrap();
                Scheduler::send_function_call(&mut workers, workerid, job);
              }
              None => {
                job_queue.push_back(job);
              }
            }
          },
          Event::Obj(newobjref) => {
            // TODO: do this with a binary search
            for &(workerid, objref) in pull_queue.iter() {
              if objref == newobjref {
                Scheduler::send_pull_request(&mut workers, workerid, objref);
              }
            }
          },
          Event::Pull(workerid, objref) => {
            if self.objtable.lock().unwrap()[objref as usize].len() > 0 {
              Scheduler::send_pull_request(&mut workers, workerid, objref);
            } else {
              pull_queue.push_back((workerid, objref));
            }
          },
          Event::Register(workerid, incoming) => {
            while workers.len() < workerid + 1 {
              workers.push(incoming.clone());
            }
            workers[workerid] = incoming;
          },
          Event::Debug(workerid) => {
            Scheduler::send_debugging_info(&workers[workerid], &worker_queue, &job_queue);
          }
        }
      }
    });
  }
}

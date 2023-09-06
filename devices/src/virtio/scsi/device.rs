// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::rc::Rc;

use anyhow::Context;
use base::error;
use base::Event;
use base::WorkerThread;
use cros_async::EventAsync;
use cros_async::Executor;
use cros_async::ExecutorKind;
use disk::DiskFile;
use futures::pin_mut;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use virtio_sys::virtio_scsi::virtio_scsi_cmd_req;
use virtio_sys::virtio_scsi::virtio_scsi_cmd_resp;
use virtio_sys::virtio_scsi::virtio_scsi_config;
use virtio_sys::virtio_scsi::virtio_scsi_event;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_CDB_DEFAULT_SIZE;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_SENSE_DEFAULT_SIZE;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_S_BAD_TARGET;
use vm_memory::GuestMemory;
use zerocopy::AsBytes;

use crate::virtio::async_utils;
use crate::virtio::block::sys::get_seg_max;
use crate::virtio::copy_config;
use crate::virtio::DescriptorChain;
use crate::virtio::DeviceType as VirtioDeviceType;
use crate::virtio::Interrupt;
use crate::virtio::Queue;
use crate::virtio::Reader;
use crate::virtio::VirtioDevice;
use crate::virtio::Writer;

// The following values reflects the virtio v1.2 spec:
// <https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-3470004>

// Should have one controlq, one eventq, and at least one request queue.
const MINIMUM_NUM_QUEUES: u16 = 3;
// Max channel should be 0.
const DEFAULT_MAX_CHANNEL: u16 = 0;
// Max target should be less than or equal to 255.
const DEFAULT_MAX_TARGET: u16 = 255;
// Max lun should be less than or equal to 16383
const DEFAULT_MAX_LUN: u32 = 16383;

const DEFAULT_QUEUE_SIZE: u16 = 256;

// The maximum number of linked commands.
const MAX_CMD_PER_LUN: u32 = 128;
// We set the maximum transfer size hint to 0xffff: 2^16 * 512 ~ 34mb.
const MAX_SECTORS: u32 = 0xffff;

/// Virtio device for exposing SCSI command operations on a host file.
pub struct Device {
    // Bitmap of virtio-scsi feature bits.
    avail_features: u64,
    // Represents the image on disk.
    disk_image: Option<Box<dyn DiskFile>>,
    // Sizes for the virtqueue.
    queue_sizes: Vec<u16>,
    // The maximum number of segments that can be in a command.
    seg_max: u32,
    // The size of the sense data.
    sense_size: u32,
    // The byte size of the CDB that the driver will write.
    cdb_size: u32,
    executor_kind: ExecutorKind,
    worker_threads: Vec<WorkerThread<()>>,
}

impl Device {
    /// Creates a virtio-scsi device.
    pub fn new(disk_image: Box<dyn DiskFile>, base_features: u64) -> Self {
        // b/300560198: Support feature bits in virtio-scsi.
        Self {
            avail_features: base_features,
            disk_image: Some(disk_image),
            queue_sizes: vec![DEFAULT_QUEUE_SIZE; MINIMUM_NUM_QUEUES as usize],
            seg_max: get_seg_max(DEFAULT_QUEUE_SIZE),
            sense_size: VIRTIO_SCSI_SENSE_DEFAULT_SIZE,
            cdb_size: VIRTIO_SCSI_CDB_DEFAULT_SIZE,
            executor_kind: ExecutorKind::default(),
            worker_threads: vec![],
        }
    }

    fn build_config_space(&self) -> virtio_scsi_config {
        virtio_scsi_config {
            num_queues: (self.queue_sizes.len() as u32),
            seg_max: self.seg_max,
            max_sectors: MAX_SECTORS,
            cmd_per_lun: MAX_CMD_PER_LUN,
            event_info_size: std::mem::size_of::<virtio_scsi_event>() as u32,
            sense_size: self.sense_size,
            cdb_size: self.cdb_size,
            max_channel: DEFAULT_MAX_CHANNEL,
            max_target: DEFAULT_MAX_TARGET,
            max_lun: DEFAULT_MAX_LUN,
        }
    }

    async fn execute_request(
        reader: &mut Reader,
        resp_writer: &mut Writer,
    ) -> anyhow::Result<usize> {
        #[allow(unused_variables)]
        // TODO(b/301011017): Cope with the configurable cdb size. We would need to define
        // something like virtio_scsi_cmd_req_header.
        let req_header = reader
            .read_obj::<virtio_scsi_cmd_req>()
            .context("failed to read virtio_scsi_cmd_req from virtqueue")?;
        // TODO(b/300042376): Return a proper response. For now, we always reply
        // VIRTIO_SCSI_S_BAD_TARGET to pretend that we do not have any SCSI devices corresponding
        // to the provided LUN.
        let resp = virtio_scsi_cmd_resp {
            response: VIRTIO_SCSI_S_BAD_TARGET as u8,
            ..Default::default()
        };
        resp_writer
            .write_all(resp.as_bytes())
            .context("failed to write virtio_scsi_cmd_resp to virtqueue")?;
        Ok(resp_writer.bytes_written())
    }
}

impl VirtioDevice for Device {
    fn keep_rds(&self) -> Vec<base::RawDescriptor> {
        self.disk_image
            .as_ref()
            .map(|i| i.as_raw_descriptors())
            .unwrap_or_default()
    }

    fn features(&self) -> u64 {
        self.avail_features
    }

    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Scsi
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config_space = self.build_config_space();
        copy_config(data, 0, config_space.as_bytes(), offset);
    }

    // TODO(b/301011017): implement the write_config method to make spec values writable from the
    // guest driver.

    fn activate(
        &mut self,
        _mem: GuestMemory,
        interrupt: Interrupt,
        queues: BTreeMap<usize, Queue>,
    ) -> anyhow::Result<()> {
        let executor_kind = self.executor_kind;
        let worker_thread = WorkerThread::start("virtio_scsi", move |kill_evt| {
            let ex =
                Executor::with_executor_kind(executor_kind).expect("Failed to create an executor");
            if let Err(err) = ex
                .run_until(run_worker(&ex, interrupt, queues, kill_evt))
                .expect("run_until failed")
            {
                error!("run_worker failed: {err}");
            }
        });
        self.worker_threads.push(worker_thread);
        Ok(())
    }
}

async fn run_worker(
    ex: &Executor,
    interrupt: Interrupt,
    mut queues: BTreeMap<usize, Queue>,
    kill_evt: Event,
) -> anyhow::Result<()> {
    let kill = async_utils::await_and_exit(ex, kill_evt).fuse();
    pin_mut!(kill);

    let resample = async_utils::handle_irq_resample(ex, interrupt.clone()).fuse();
    pin_mut!(resample);

    let request_queue = queues
        .remove(&2)
        .context("request queue should be present")?;
    let kick_evt = request_queue
        .event()
        .try_clone()
        .expect("Failed to clone queue event");
    let queue_handler = handle_queue(
        Rc::new(RefCell::new(request_queue)),
        EventAsync::new(kick_evt, ex).expect("Failed to create async event for queue"),
        interrupt.clone(),
    )
    .fuse();
    pin_mut!(queue_handler);

    futures::select! {
        _ = queue_handler => anyhow::bail!("queue handler exited unexpectedly"),
        r = resample => return r.context("failed to resample an irq value"),
        r = kill => return r.context("failed to wait on the kill event"),
    };
}

async fn handle_queue(queue: Rc<RefCell<Queue>>, evt: EventAsync, interrupt: Interrupt) {
    let mut background_tasks = FuturesUnordered::new();
    let evt_future = evt.next_val().fuse();
    pin_mut!(evt_future);
    loop {
        futures::select! {
            _ = background_tasks.next() => continue,
            res = evt_future => {
                evt_future.set(evt.next_val().fuse());
                if let Err(e) = res {
                    error!("Failed to read the next queue event: {e}");
                    continue;
                }
            }
        }
        while let Some(chain) = queue.borrow_mut().pop() {
            background_tasks.push(process_one_chain(&queue, chain, &interrupt));
        }
    }
}

async fn process_one_chain(
    queue: &RefCell<Queue>,
    mut avail_desc: DescriptorChain,
    interrupt: &Interrupt,
) {
    let len = process_one_request(&mut avail_desc).await;
    let mut queue = queue.borrow_mut();
    queue.add_used(avail_desc, len as u32);
    queue.trigger_interrupt(interrupt);
}

async fn process_one_request(avail_desc: &mut DescriptorChain) -> usize {
    let reader = &mut avail_desc.reader;
    let resp_writer = &mut avail_desc.writer;
    match Device::execute_request(reader, resp_writer).await {
        Ok(n) => n,
        Err(e) => {
            error!("request failed: {:#}", e);
            0
        }
    }
}
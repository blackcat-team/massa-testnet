use super::super::{
    block_database::*, config::ConsensusConfig, consensus_controller::*, random_selector::*,
    timeslots::*,
};
use crate::protocol::protocol_controller::{
    NodeId, ProtocolController, ProtocolEvent, ProtocolEventType,
};
use tokio::stream::StreamExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::sleep_until;

#[derive(Clone, Debug)]
pub enum ConsensusCommand {
    CreateBlock(String),
}

pub struct ConsensusWorker<ProtocolControllerT: ProtocolController + 'static> {
    cfg: ConsensusConfig,
    protocol_controller: ProtocolControllerT,
    block_db: BlockDatabase,
    controller_command_rx: Receiver<ConsensusCommand>,
    controller_event_tx: Sender<ConsensusEvent>,
}

impl<ProtocolControllerT: ProtocolController + 'static> ConsensusWorker<ProtocolControllerT> {
    pub fn new(
        cfg: ConsensusConfig,
        protocol_controller: ProtocolControllerT,
        block_db: BlockDatabase,
        controller_command_rx: Receiver<ConsensusCommand>,
        controller_event_tx: Sender<ConsensusEvent>,
    ) -> ConsensusWorker<ProtocolControllerT> {
        ConsensusWorker {
            cfg,
            protocol_controller,
            block_db,
            controller_command_rx,
            controller_event_tx,
        }
    }

    pub async fn run_loop(mut self) {
        let (mut next_slot_thread, mut next_slot_number) = get_current_latest_block_slot(
            self.cfg.thread_count,
            self.cfg.t0_millis,
            self.cfg.genesis_timestamp_millis,
        )
        .map_or((0u8, 0u64), |(cur_thread, cur_slot)| {
            get_next_block_slot(self.cfg.thread_count, cur_thread, cur_slot)
        });
        let mut next_slot_timer = sleep_until(estimate_instant_from_timestamp(
            get_block_slot_timestamp_millis(
                self.cfg.thread_count,
                self.cfg.t0_millis,
                self.cfg.genesis_timestamp_millis,
                next_slot_thread,
                next_slot_number,
            ),
        ));
        let seed = vec![0u8; 32]; // TODO temporary
        let participants_weights = vec![1u64; self.cfg.nodes.len()];
        let mut selector = RandomSelector::new(&seed, self.cfg.thread_count, participants_weights);

        loop {
            tokio::select! {
                // listen consensus commands
                res = self.controller_command_rx.next() => match res {
                    Some(cmd) => self.process_consensus_command(cmd).await,
                    None => break  // finished
                },

                // slot timer
                _ = &mut next_slot_timer => {
                    massa_trace!("slot_timer", {
                        "slot_thread": next_slot_thread,
                        "slot_number": next_slot_number
                    });

                    // check if it is our turn to create a block
                    let block_creator = selector.draw(next_slot_thread, next_slot_number);
                    if block_creator == self.cfg.current_node_index {
                        // TODO create new block at slot (next_slot_thread, next_slot_number)
                    }

                    // reset timer for next slot
                    (next_slot_thread, next_slot_number) = get_next_block_slot(self.cfg.thread_count, next_slot_thread, next_slot_number);
                    next_slot_timer = sleep_until(estimate_instant_from_timestamp(
                        get_block_slot_timestamp_millis(
                            self.cfg.thread_count,
                            self.cfg.t0_millis,
                            self.cfg.genesis_timestamp_millis,
                            next_slot_thread,
                            next_slot_number,
                        ),
                    ));
                }

                // listen protocol controller events
                ProtocolEvent(source_node_id, event) = self.protocol_controller.wait_event() =>
                    self.process_protocol_event(source_node_id, event).await,
            }
        }

        // end loop
        self.protocol_controller.stop().await;
    }

    async fn process_consensus_command(&mut self, cmd: ConsensusCommand) {
        match cmd {
            ConsensusCommand::CreateBlock(block) => {
                massa_trace!("created_block", { "block": block });
                let block = self.block_db.create_block(block);
                if self.block_db.acknowledge_new_block(&block) {
                    self.protocol_controller
                        .propagate_block(&block, None, None)
                        .await;
                }
            }
        }
    }

    async fn process_protocol_event(&mut self, source_node_id: NodeId, event: ProtocolEventType) {
        match event {
            ProtocolEventType::ReceivedBlock(block) => {
                if self.block_db.acknowledge_new_block(&block) {
                    massa_trace!("received_block_ok", {"source_node_id": source_node_id, "block": block});
                    self.protocol_controller
                        .propagate_block(&block, Some(source_node_id), None)
                        .await;
                } else {
                    massa_trace!("received_block_ignore", {"source_node_id": source_node_id, "block": block});
                }
            }
            ProtocolEventType::ReceivedTransaction(transaction) => {
                // todo
            }
            ProtocolEventType::AskedBlock(block_hash) => {
                for db in &self.block_db.0 {
                    if let Some(block) = db.get(&block_hash) {
                        massa_trace!("sending_block", {"dest_node_id": source_node_id, "block": block});
                        self.protocol_controller
                            .propagate_block(block, None, Some(source_node_id))
                            .await;
                    }
                }
            }
        }
    }
}

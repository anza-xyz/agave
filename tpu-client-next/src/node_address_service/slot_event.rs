use solana_clock::Slot;

#[derive(Debug, Clone)]
pub enum SlotEvent {
    Start(Slot),
    End(Slot),
}

impl SlotEvent {
    pub fn slot(&self) -> Slot {
        match self {
            SlotEvent::Start(slot) | SlotEvent::End(slot) => *slot,
        }
    }

    pub fn is_start(&self) -> bool {
        matches!(self, SlotEvent::Start(_))
    }
}

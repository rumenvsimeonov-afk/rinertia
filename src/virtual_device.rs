use anyhow::Result;
use evdev::{
    AttributeSet, BusType, EventType, InputEvent, InputId, Key, RelativeAxisType, Synchronization,
    uinput::VirtualDeviceBuilder,
};

pub struct VirtualDevice {
    device: evdev::uinput::VirtualDevice,
}

impl VirtualDevice {
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        keys.insert(Key::BTN_LEFT);

        let mut rel_axes = AttributeSet::<RelativeAxisType>::new();
        rel_axes.insert(RelativeAxisType::REL_X);
        rel_axes.insert(RelativeAxisType::REL_Y);

        let device = VirtualDeviceBuilder::new()?
            .name("rinertia Virtual Device")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0, 0, 1))
            .with_keys(&keys)?
            .with_relative_axes(&rel_axes)?
            .build()?;

        Ok(Self { device })
    }

    pub fn emit_pointer(&mut self, dx: i32, dy: i32) -> Result<()> {
        let events = [
            InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_X.0, dx),
            InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_Y.0, dy),
            InputEvent::new(EventType::SYNCHRONIZATION, Synchronization::SYN_REPORT.0, 0),
        ];
        self.device.emit(&events)?;
        Ok(())
    }
}

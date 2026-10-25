//! Over-the-air updates with a trial boot.
//!
//! An upload is written to the inactive app slot, which is then selected with
//! state `New`. espflash's bootloader has no rollback support, so the firmware
//! does it: a `New` slot boots once as a trial (`PendingVerify`) and must
//! confirm itself (a lease from the uplink) within `TRIAL_TIMEOUT`. A trial
//! that reboots, panics or hangs (the watchdog resets it) without confirming
//! is marked `Invalid` and the previous slot boots again.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::{Mutex, MutexGuard};
use embassy_time::{Duration, Timer};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{AppPartitionSubType, PARTITION_TABLE_MAX_LEN};
use esp_storage::FlashStorage;
use log::{info, warn};

/// How long a freshly installed image has to get a lease before it is rolled back.
const TRIAL_TIMEOUT: Duration = Duration::from_secs(300);
pub const FLASH_SECTOR_SIZE: usize = 4096;
const APP_IMAGE_MAGIC_BYTE: u8 = 0xe9;
/// ESP32-C6 in the image header's chip ID field.
const CHIP_ID_ESP32C6: u16 = 0x000d;

struct Flash {
    storage: FlashStorage<'static>,
    partition_table_buffer: [u8; PARTITION_TABLE_MAX_LEN],
}

impl Flash {
    fn ota_updater(&mut self) -> Result<OtaUpdater<'_, 'static>, &'static str> {
        OtaUpdater::new(&mut self.storage, &mut self.partition_table_buffer)
            .map_err(|_| "no OTA partitions (flash over USB once)")
    }
}

static FLASH: Mutex<CriticalSectionRawMutex, Option<Flash>> = Mutex::new(None);
static IS_ON_TRIAL: AtomicBool = AtomicBool::new(false);
/// Running slot: 0 = none (single-app layout), else OTA app number + 1.
static RUNNING_SLOT: AtomicU8 = AtomicU8::new(0);

/// Running slot and whether it is still on trial, for the status page.
pub fn running_slot_and_state() -> (&'static str, &'static str) {
    let slot = match RUNNING_SLOT.load(Ordering::Relaxed) {
        0 => "single app",
        1 => "ota_0",
        2 => "ota_1",
        _ => "other",
    };
    let state = if IS_ON_TRIAL.load(Ordering::Relaxed) { "trial" } else { "confirmed" };
    (slot, state)
}

/// Check the running slot's state before anything else starts. Rolls back
/// (and resets) if the previous boot was an unconfirmed trial.
#[allow(clippy::large_stack_frames, reason = "runs once at boot, before any task, on the 80 KB main stack")]
pub fn check_at_boot(storage: FlashStorage<'static>) {
    let mut flash = Flash { storage, partition_table_buffer: [0; PARTITION_TABLE_MAX_LEN] };
    match flash.ota_updater() {
        Err(error) => warn!("OTA unavailable: {error}"),
        Ok(mut updater) => {
            let slot = updater.selected_partition().ok();
            match updater.current_ota_state() {
                Ok(OtaImageState::New) => {
                    info!("Trial boot of {slot:?}: confirming once the uplink gives us a lease");
                    let _ = updater.set_current_ota_state(OtaImageState::PendingVerify);
                    IS_ON_TRIAL.store(true, Ordering::Relaxed);
                }
                Ok(OtaImageState::PendingVerify) => {
                    warn!("{slot:?} never confirmed its trial: rolling back");
                    roll_back_and_reboot(&mut updater);
                }
                state => info!("Running {slot:?} ({state:?})"),
            }
            let slot_number = match slot {
                Some(AppPartitionSubType::Ota0) => 1,
                Some(AppPartitionSubType::Ota1) => 2,
                _ => 3,
            };
            RUNNING_SLOT.store(slot_number, Ordering::Relaxed);
        }
    }
    if let Ok(mut shared_flash) = FLASH.try_lock() {
        *shared_flash = Some(flash);
    }
}

fn roll_back_and_reboot(updater: &mut OtaUpdater<'_, 'static>) -> ! {
    let _ = updater.set_current_ota_state(OtaImageState::Invalid);
    if let Err(error) = updater.activate_next_partition() {
        warn!("Rollback failed: {error:?}");
    }
    esp_hal::system::software_reset()
}

/// Mark a trial image as good. Called once the uplink gave us a lease.
pub async fn confirm_update() {
    if !IS_ON_TRIAL.load(Ordering::Relaxed) {
        return;
    }
    let mut shared_flash = FLASH.lock().await;
    let Some(flash) = shared_flash.as_mut() else { return };
    let marked_valid = flash.ota_updater().and_then(|mut updater| {
        updater.set_current_ota_state(OtaImageState::Valid).map_err(|_| "write failed")
    });
    match marked_valid {
        Ok(()) => {
            IS_ON_TRIAL.store(false, Ordering::Relaxed);
            info!("Update confirmed");
        }
        Err(error) => warn!("Confirming the update failed: {error}"),
    }
}

/// Roll a trial back if it doesn't confirm itself in time.
#[embassy_executor::task]
pub async fn roll_back_unconfirmed_trial() {
    if !IS_ON_TRIAL.load(Ordering::Relaxed) {
        return;
    }
    Timer::after(TRIAL_TIMEOUT).await;
    if IS_ON_TRIAL.load(Ordering::Relaxed) {
        warn!("Trial not confirmed after {} s", TRIAL_TIMEOUT.as_secs());
        let mut shared_flash = FLASH.lock().await;
        if let Some(Ok(mut updater)) = shared_flash.as_mut().map(Flash::ota_updater) {
            roll_back_and_reboot(&mut updater);
        }
    }
}

/// An upload in progress: writes chunks to the inactive slot.
pub struct Upload {
    flash: MutexGuard<'static, CriticalSectionRawMutex, Option<Flash>>,
    image_size: usize,
    bytes_written: usize,
}

impl Upload {
    /// Start writing an image of `image_size` bytes. Fails if another upload
    /// runs, there are no OTA partitions, or the image can't fit.
    pub fn start(image_size: usize) -> Result<Self, &'static str> {
        let mut flash = FLASH.try_lock().map_err(|_| "another update is in progress")?;
        let mut updater = flash.as_mut().ok_or("flash unavailable")?.ota_updater()?;
        let (inactive_slot, slot_name) = updater.next_partition().map_err(|_| "no inactive slot")?;
        if image_size == 0 || image_size > inactive_slot.capacity() {
            return Err("image doesn't fit the app slot");
        }
        info!("Writing a {image_size} B update to {slot_name:?}");
        Ok(Self { flash, image_size, bytes_written: 0 })
    }

    /// Write the next chunk (a full sector, or the end of the image).
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        if self.bytes_written == 0 {
            check_image_header(chunk)?;
        }
        if self.bytes_written + chunk.len() > self.image_size {
            return Err("more data than announced");
        }
        let mut updater = self.flash.as_mut().ok_or("flash unavailable")?.ota_updater()?;
        let (mut inactive_slot, _) = updater.next_partition().map_err(|_| "no inactive slot")?;
        inactive_slot.write(self.bytes_written as u32, chunk).map_err(|_| "flash write failed")?;
        self.bytes_written += chunk.len();
        let crossed_a_64_kb_mark = self.bytes_written % (64 * 1024) < chunk.len();
        if crossed_a_64_kb_mark {
            info!("Update: {} / {} KB", self.bytes_written / 1024, self.image_size / 1024);
        }
        Ok(())
    }

    /// Select the new image for the next boot, as a trial.
    pub fn finish(mut self) -> Result<(), &'static str> {
        if self.bytes_written != self.image_size {
            return Err("upload incomplete");
        }
        let mut updater = self.flash.as_mut().ok_or("flash unavailable")?.ota_updater()?;
        updater.activate_next_partition().map_err(|_| "selecting the new slot failed")?;
        updater.set_current_ota_state(OtaImageState::New).map_err(|_| "marking the new slot failed")?;
        info!("Update written; rebooting into it");
        Ok(())
    }
}

/// Refuse anything that isn't an ESP32-C6 app image.
fn check_image_header(first_chunk: &[u8]) -> Result<(), &'static str> {
    if first_chunk.len() < 24 || first_chunk[0] != APP_IMAGE_MAGIC_BYTE {
        return Err("not an ESP app image (use espflash save-image)");
    }
    let chip_id = u16::from_le_bytes([first_chunk[12], first_chunk[13]]);
    if chip_id != CHIP_ID_ESP32C6 {
        return Err("image is not for the ESP32-C6");
    }
    Ok(())
}

/// Reboot after giving the HTTP response a moment to leave.
pub async fn reboot_soon() -> ! {
    Timer::after(Duration::from_millis(500)).await;
    esp_hal::system::software_reset()
}

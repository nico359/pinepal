// SPDX-License-Identifier: GPL-3.0-or-later
// Legacy Nordic DFU over BLE, as InfiniTime implements it. Ported from
// pinetime-furios (same bluer version, same wire protocol).
//
// Every Control Point response is validated and any mismatch/timeout aborts —
// a partial/failed transfer leaves the watch on its current firmware, and
// MCUboot reverts an unvalidated image, so aborting is always the safe choice.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::time::timeout;

const STEP_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 20;
const RECEIPT_INTERVAL: u8 = 10;

/// Control Point: write-with-response (responses arrive as notifications).
fn cp() -> CharacteristicWriteRequest {
    CharacteristicWriteRequest { op_type: WriteOp::Request, ..Default::default() }
}
/// Packet: write-without-response (flow-controlled by packet receipts).
fn pkt() -> CharacteristicWriteRequest {
    CharacteristicWriteRequest { op_type: WriteOp::Command, ..Default::default() }
}

/// Run a full DFU. `progress` is called with 0..=100 as the image transfers.
pub async fn run_dfu(
    control_point: &Characteristic,
    packet: &Characteristic,
    bin: &[u8],
    dat: &[u8],
    mut progress: impl FnMut(u8),
) -> Result<()> {
    anyhow::ensure!(!bin.is_empty() && !dat.is_empty(), "empty firmware package");
    let mut notifs = control_point
        .notify()
        .await
        .context("subscribing to control point")?
        .boxed();

    // 1. Start DFU for an application image.
    control_point.write_ext(&[0x01, 0x04], &cp()).await.context("start DFU")?;
    // Image sizes: 8 zero bytes (softdevice + bootloader) then app size (u32 LE).
    let mut sizes = vec![0u8; 8];
    sizes.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    packet.write_ext(&sizes, &pkt()).await.context("writing image size")?;
    expect(&mut notifs, &[0x10, 0x01, 0x01]).await.context("start DFU")?;

    // 2. Init packet (.dat).
    control_point.write_ext(&[0x02, 0x00], &cp()).await.context("init begin")?;
    for chunk in dat.chunks(CHUNK) {
        packet.write_ext(chunk, &pkt()).await.context("writing init packet")?;
    }
    control_point.write_ext(&[0x02, 0x01], &cp()).await.context("init end")?;
    expect(&mut notifs, &[0x10, 0x02, 0x01]).await.context("init packet")?;

    // 3. Firmware image, flow-controlled by packet receipts.
    control_point.write_ext(&[0x08, RECEIPT_INTERVAL], &cp()).await.context("set receipts")?;
    control_point.write_ext(&[0x03], &cp()).await.context("receive image")?;

    let total = bin.len();
    let mut sent = 0usize;
    let mut since_receipt = 0u8;
    progress(0);
    for chunk in bin.chunks(CHUNK) {
        packet.write_ext(chunk, &pkt()).await.context("writing firmware chunk")?;
        sent += chunk.len();
        since_receipt += 1;
        if since_receipt == RECEIPT_INTERVAL {
            since_receipt = 0;
            let receipt = next(&mut notifs).await.context("packet receipt")?;
            if receipt.first() != Some(&0x11) || receipt.len() < 5 {
                bail!("unexpected packet receipt: {receipt:02x?}");
            }
            let acked = u32::from_le_bytes([receipt[1], receipt[2], receipt[3], receipt[4]]) as usize;
            if acked != sent {
                bail!("receipt mismatch: watch acked {acked} of {sent} bytes");
            }
            progress(((sent * 100) / total).min(99) as u8);
        }
    }
    expect(&mut notifs, &[0x10, 0x03, 0x01]).await.context("image transfer")?;

    // 4. Validate then activate + reset.
    control_point.write_ext(&[0x04], &cp()).await.context("validate")?;
    expect(&mut notifs, &[0x10, 0x04, 0x01]).await.context("validate image")?;
    progress(100);
    // The watch resets the instant it receives "activate", before it can send a
    // write response — so a missing ack / disconnect here is the success signal,
    // not a failure. The image is already validated at this point.
    if let Err(e) = control_point.write_ext(&[0x05], &cp()).await {
        log::debug!("activate write returned {e:#} (expected: watch is resetting)");
    }
    Ok(())
}

async fn next(notifs: &mut BoxStream<'_, Vec<u8>>) -> Result<Vec<u8>> {
    match timeout(STEP_TIMEOUT, notifs.next()).await {
        Ok(Some(v)) => Ok(v),
        Ok(None) => bail!("control point notifications ended unexpectedly"),
        Err(_) => bail!("timed out waiting for a control point response"),
    }
}

async fn expect(notifs: &mut BoxStream<'_, Vec<u8>>, expected: &[u8]) -> Result<()> {
    let got = next(notifs).await?;
    if got.len() >= expected.len() && got[..expected.len()] == *expected {
        Ok(())
    } else {
        bail!("expected control point response {expected:02x?}, got {got:02x?}")
    }
}

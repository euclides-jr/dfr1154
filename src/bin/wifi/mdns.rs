use embassy_net::{
    IpEndpoint, Ipv4Address, Stack,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_time::Timer;

const GROUP: Ipv4Address = Ipv4Address::new(224, 0, 0, 251);
const PORT: u16 = 5353;
const HOST: &[u8; 15] = b"\x07dfrobot\x05local\x00";

#[embassy_executor::task]
pub async fn respond(stack: Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY];
    let mut tx_meta = [PacketMetadata::EMPTY];
    let mut rx = [0; 512];
    let mut tx = [0; 64];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    if let Err(error) = socket.bind(PORT) {
        log::warn!("mDNS bind failed: {:?}", error);
        return;
    }
    socket.set_hop_limit(Some(255));

    let mut query = [0; 512];
    let mut response = [0; 64];
    loop {
        stack.wait_config_up().await;
        if !stack.has_multicast_group(GROUP)
            && let Err(error) = stack.join_multicast_group(GROUP)
        {
            log::warn!("mDNS multicast join failed: {:?}", error);
            Timer::after_secs(2).await;
            continue;
        }
        let Ok((len, source)) = socket.recv_from(&mut query).await else {
            continue;
        };
        let Some(config) = stack.config_v4() else {
            continue;
        };
        let legacy = source.endpoint.port != PORT;
        let Some(len) = answer(
            &query[..len],
            config.address.address().octets(),
            legacy,
            &mut response,
        ) else {
            continue;
        };
        let destination = if legacy {
            source.endpoint
        } else {
            IpEndpoint::new(GROUP.into(), PORT)
        };
        let _ = socket.send_to(&response[..len], destination).await;
    }
}

fn answer(query: &[u8], ip: [u8; 4], legacy: bool, out: &mut [u8; 64]) -> Option<usize> {
    let header = query.get(..12)?;
    // Ignore responses, nonstandard opcodes, and truncated queries.
    if header[2] & 0xfa != 0 {
        return None;
    }
    let mut offset = 12;
    for _ in 0..u16::from_be_bytes([header[4], header[5]]) {
        let (end, is_host) = read_name(query, offset)?;
        let fields = query.get(end..end + 4)?;
        offset = end + 4;
        let qtype = u16::from_be_bytes([fields[0], fields[1]]);
        let qclass = u16::from_be_bytes([fields[2], fields[3]]) & 0x7fff;
        if !is_host || !matches!(qtype, 1 | 255) || !matches!(qclass, 1 | 255) {
            continue;
        }
        out[..12].fill(0);
        out[2] = 0x84; // Authoritative response.
        out[7] = 1;
        let mut pos = 12;
        if legacy {
            out[..2].copy_from_slice(&header[..2]);
            out[5] = 1;
            out[pos..pos + HOST.len()].copy_from_slice(HOST);
            pos += HOST.len();
            out[pos..pos + 4].copy_from_slice(fields);
            pos += 4;
        }
        out[pos..pos + HOST.len()].copy_from_slice(HOST);
        pos += HOST.len();
        let class = if legacy { 1u16 } else { 0x8001 };
        let ttl = if legacy { 10u32 } else { 120 };
        out[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // A record.
        out[pos + 2..pos + 4].copy_from_slice(&class.to_be_bytes());
        out[pos + 4..pos + 8].copy_from_slice(&ttl.to_be_bytes());
        out[pos + 8..pos + 10].copy_from_slice(&4u16.to_be_bytes());
        out[pos + 10..pos + 14].copy_from_slice(&ip);
        return Some(pos + 14);
    }
    None
}

fn read_name(packet: &[u8], mut pos: usize) -> Option<(usize, bool)> {
    let mut end = None;
    let mut matched = 0;
    let mut is_host = true;
    // Bound the work even if the packet contains cyclic compression pointers.
    for _ in 0..128 {
        let len = *packet.get(pos)?;
        if len & 0xc0 == 0xc0 {
            let low = *packet.get(pos + 1)?;
            end.get_or_insert(pos + 2);
            pos = u16::from_be_bytes([len & 0x3f, low]) as usize;
            continue;
        }
        if len > 63 {
            return None;
        }
        let next = pos + 1 + usize::from(len);
        let label = packet.get(pos..next)?;
        is_host &= HOST
            .get(matched..matched + label.len())
            .is_some_and(|expected| expected.eq_ignore_ascii_case(label));
        matched += label.len();
        if len == 0 {
            return Some((end.unwrap_or(next), is_host && matched == HOST.len()));
        }
        pos = next;
    }
    None
}

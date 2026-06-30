use super::super::{detect_packet_layout, demux_ts, STREAM_TYPE_MPEG2_VIDEO, TS_PACKET};
use super::ts_pkt;

#[test]
fn detects_plain_ts_layout() {
    let mut buf = Vec::with_capacity(3 * TS_PACKET);
    for _ in 0..3 {
        let pkt = ts_pkt(0x1FFF, false, 0b01, &[]);
        buf.extend_from_slice(&pkt);
    }
    let (count, stride, prefix) = detect_packet_layout(&buf).unwrap();
    assert_eq!((count, stride, prefix), (3, 188, 0));
}

#[test]
fn parses_minimal_pat_pmt_and_reassembles_one_sample() {
    // Build a PAT pointing at PMT=0x100, a PMT listing video PID=0x200
    // stream_type=MPEG-2, then a single PES packet carrying 16 bytes
    // of video ES.

    // PAT section (we skip CRC correctness — the parser only uses
    // section_length to decide where to stop).
    let mut pat = vec![0u8; 0];
    pat.push(0x00); // table_id
    let section_length: usize = 5 + 4 + 4; // 5 header bytes (after len) + 1 program + CRC
    pat.push(0xB0 | ((section_length >> 8) & 0x0F) as u8);
    pat.push((section_length & 0xFF) as u8);
    pat.extend_from_slice(&[0x00, 0x01, 0xC1, 0x00, 0x00]); // tsid, ver/current, secno, lastno
    pat.extend_from_slice(&[0x00, 0x01]); // program_number = 1
    pat.extend_from_slice(&[0xE1, 0x00]); // reserved + PMT PID = 0x100
    pat.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder

    // PAT packet payload = [pointer_field=0, section...]
    let mut pat_payload = vec![0u8];
    pat_payload.extend_from_slice(&pat);
    let pat_pkt = ts_pkt(0x0000, true, 0b01, &pat_payload);

    // PMT section.
    let mut pmt = vec![0u8; 0];
    pmt.push(0x02);
    let pmt_sec_len: usize = 9 + 5 + 4; // program_number..pil(9) + 1 stream entry(5) + CRC(4)
    pmt.push(0xB0 | ((pmt_sec_len >> 8) & 0x0F) as u8);
    pmt.push((pmt_sec_len & 0xFF) as u8);
    pmt.extend_from_slice(&[0x00, 0x01, 0xC1, 0x00, 0x00]); // prog, ver/current, sec/last
    pmt.extend_from_slice(&[0xE2, 0x00]); // PCR PID = 0x200
    pmt.extend_from_slice(&[0xF0, 0x00]); // program_info_length = 0
    pmt.extend_from_slice(&[STREAM_TYPE_MPEG2_VIDEO, 0xE2, 0x00, 0xF0, 0x00]); // stream entry
    pmt.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
    let mut pmt_payload = vec![0u8];
    pmt_payload.extend_from_slice(&pmt);
    let pmt_pkt = ts_pkt(0x0100, true, 0b01, &pmt_payload);

    // Two PES packets, each 16 bytes of ES, so the reassembler's
    // PUSI-flush path is exercised. Real MPEG-TS files also set
    // PES_packet_length which bounds the first one, but packet_length=0
    // ("unbounded") is also legal for MPEG-2 video PES, which is what
    // we emit here — termination comes from the next PUSI.
    let make_pes = |byte: u8| {
        let mut pes = vec![0u8, 0u8, 1u8]; // start code
        pes.push(0xE0); // stream_id video
        pes.extend_from_slice(&[0u8, 0u8]); // packet_length=0
        pes.push(0x80);
        pes.push(0x80); // PTS_DTS_flags = 10
        pes.push(5); // PES_header_data_length
        pes.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]); // PTS=0
        pes.extend_from_slice(&[byte; 16]);
        pes
    };
    let pes_pkt_a = ts_pkt(0x0200, true, 0b01, &make_pes(0xAA));
    let pes_pkt_b = ts_pkt(0x0200, true, 0b01, &make_pes(0xBB));

    let mut buf = Vec::new();
    buf.extend_from_slice(&pat_pkt);
    buf.extend_from_slice(&pmt_pkt);
    buf.extend_from_slice(&pes_pkt_a);
    buf.extend_from_slice(&pes_pkt_b);
    // Trailing null packet so detect_packet_layout sees a sync run.
    buf.extend_from_slice(&ts_pkt(0x1FFF, false, 0b01, &[]));

    let d = demux_ts(&buf).expect("demux");
    assert_eq!(d.codec, "mpeg2");
    // We should have reassembled two samples (the first flushed when
    // the second PUSI arrives). Sample A carries the 16 AU bytes
    // plus whatever TS padding trailed the PES header — the
    // demuxer does not know the bound, so exact byte-for-byte
    // comparison needs packet_length support (future). For now
    // assert: right sample count, correct leading bytes.
    assert_eq!(d.samples.len(), 2);
    assert_eq!(&d.samples[0][..16], &[0xAA; 16]);
    assert_eq!(&d.samples[1][..16], &[0xBB; 16]);
}

#[test]
fn rejects_file_with_no_sync() {
    let garbage = vec![0u8; TS_PACKET * 3];
    assert!(demux_ts(&garbage).is_err());
}

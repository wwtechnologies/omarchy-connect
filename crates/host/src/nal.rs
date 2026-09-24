//! Annex-B access-unit splitter used by the VAAPI ffmpeg pipe.
//!
//! libx264 returns one access unit per `encode` call. `h264_vaapi` writes a
//! raw byte stream, so this groups NALs into access units. An access unit is
//! emitted only once the next one has started, which keeps the last unit in
//! the buffer until its following start code arrives.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub fn push_bytes(buf: &mut Vec<u8>, incoming: &[u8]) -> Vec<AccessUnit> {
    if !incoming.is_empty() {
        buf.extend_from_slice(incoming);
    }
    let starts = start_codes(buf);
    if starts.len() < 2 {
        return Vec::new();
    }
    let mut groups: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut current: Vec<(usize, usize)> = Vec::new();
    let mut current_has_vcl = false;
    for window in starts.windows(2) {
        let (start, end) = (window[0], window[1]);
        let kind = nal_type(buf, start);
        let vcl = (1..=5).contains(&kind);
        let boundary = kind == 9 || kind == 7 || (vcl && current_has_vcl);
        if boundary && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
            current_has_vcl = false;
        }
        if vcl {
            current_has_vcl = true;
        }
        current.push((start, end));
    }
    let keep_from = current
        .first()
        .map(|(start, _)| *start)
        .unwrap_or(starts[starts.len() - 1]);
    let mut units = Vec::with_capacity(groups.len());
    for group in groups {
        let begin = group[0].0;
        let end = group.last().map(|(_, end)| *end).unwrap_or(begin);
        let data = buf[begin..end].to_vec();
        let keyframe = group.iter().any(|(start, _)| {
            let kind = nal_type(buf, *start);
            kind == 5 || kind == 7
        });
        if !data.is_empty() {
            units.push(AccessUnit { keyframe, data });
        }
    }
    if keep_from > 0 && keep_from <= buf.len() {
        buf.drain(..keep_from);
    }
    units
}

fn start_codes(buf: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            out.push(i);
            i += 3;
            continue;
        }
        if i + 3 < buf.len() && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1
        {
            out.push(i);
            i += 4;
            continue;
        }
        i += 1;
    }
    out
}

fn nal_type(buf: &[u8], start: usize) -> u8 {
    let header = if start + 2 < buf.len() && buf[start + 2] == 1 {
        start + 3
    } else {
        start + 4
    };
    if header < buf.len() {
        buf[header] & 0x1f
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 1, kind];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn splits_on_the_next_access_unit() {
        let mut stream = Vec::new();
        stream.extend(nal(7, &[1, 2]));
        stream.extend(nal(8, &[3]));
        stream.extend(nal(5, &[9, 9]));
        stream.extend(nal(1, &[4, 4]));
        stream.extend(nal(1, &[5]));
        let mut buf = Vec::new();
        let units = push_bytes(&mut buf, &stream);
        assert_eq!(units.len(), 1, "the open P-slice stays buffered");
        assert!(units[0].keyframe);
        assert!(!buf.is_empty());
        let more = push_bytes(&mut buf, &nal(1, &[6]));
        assert_eq!(more.len(), 1);
        assert!(!more[0].keyframe);
    }
}

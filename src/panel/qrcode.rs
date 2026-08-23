//! Builds the pairing QR: the V3/V4 WebSocket URL, wrapped in the
//! DG-LAB APP's deep link for that protocol, rendered as an inline SVG.

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use qrcode::render::svg;
use qrcode::QrCode;

pub fn ws_url(scheme: &str, host: &str, v3_port: u16, controller_id: &str) -> String {
    format!("{scheme}://{host}:{v3_port}/{controller_id}")
}

/// V4's pairing form uses `?tid=` under the relay's configured path
/// prefix (`ws://host:port<prefix>/?tid=<controllerId>`), per dglab-kit's
/// "生成 APP 配对二维码" section -- unlike V3, there's no path-tail form.
/// `prefix` always starts with `/` (see `v4::config::normalize_prefix`).
pub fn v4_ws_url(scheme: &str, host: &str, v4_port: u16, prefix: &str, controller_id: &str) -> String {
    let path = if prefix == "/" { "" } else { prefix };
    format!("{scheme}://{host}:{v4_port}{path}/?tid={controller_id}")
}

/// encodeURIComponent's unreserved set: alphanumerics plus `- _ . ! ~ * ' ( )`.
const ENCODE_URI_COMPONENT_SAFE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// dglab-kit's documented V4 deep link, percent-encoded exactly as
/// `dglab-kit`'s own README example shows -- confirmed against a real
/// DG-LAB 4 APP (unlike V3's scheme below, which needed correcting away
/// from what its reference docs showed after real-device testing proved
/// them wrong, V4's documented `encodeURIComponent` form paired
/// successfully as-is on the first live test).
pub fn v4_pairing_deep_link(ws_url: &str) -> String {
    let encoded = utf8_percent_encode(ws_url, ENCODE_URI_COMPONENT_SAFE);
    format!("https://dungeon-lab.cn/s/?v=1&action=socket&url={encoded}")
}

/// The reference TS server's own README shows this link built with
/// `encodeURIComponent(wsUrl)`, but real-device testing against the
/// actual DG-LAB APP shows it expects the raw `ws://...` URL verbatim
/// after the `#DGLAB-SOCKET#` marker -- percent-encoding it (as the docs
/// suggest) makes the APP fail to pair. Trusting the observed on-device
/// behavior over the docs here.
pub fn pairing_deep_link(ws_url: &str) -> String {
    format!("https://www.dungeon-lab.com/app-download.php#DGLAB-SOCKET#{ws_url}")
}

/// Renders `data` as an inline SVG `<svg>...</svg>` string, suitable for
/// direct `innerHTML` embedding in the browser. Returns an error only if
/// `data` is too large to encode as a QR code (never expected in
/// practice -- `data` here is always a short, fixed-shape pairing URL).
pub fn render_svg(data: &str) -> Result<String, qrcode::types::QrError> {
    let code = QrCode::new(data.as_bytes())?;
    let rendered = code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build();
    // The renderer prepends an `<?xml ...?>` prolog, which isn't valid
    // when injected into an existing HTML document -- keep only the
    // `<svg>...</svg>` element itself.
    let svg_start = rendered.find("<svg").unwrap_or(0);
    Ok(rendered[svg_start..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_uses_path_tail_target_id_form() {
        assert_eq!(
            ws_url("ws", "127.0.0.1", 10002, "abc-123"),
            "ws://127.0.0.1:10002/abc-123"
        );
    }

    #[test]
    fn v4_ws_url_uses_query_tid_form_and_avoids_double_slash_at_root() {
        assert_eq!(v4_ws_url("ws", "127.0.0.1", 10001, "/", "abc-123"), "ws://127.0.0.1:10001/?tid=abc-123");
        assert_eq!(v4_ws_url("ws", "127.0.0.1", 10001, "/v4", "abc-123"), "ws://127.0.0.1:10001/v4/?tid=abc-123");
    }

    #[test]
    fn v4_pairing_deep_link_percent_encodes_the_ws_url() {
        let link = v4_pairing_deep_link("ws://192.168.1.133:10001/?tid=abc-123");
        assert_eq!(
            link,
            "https://dungeon-lab.cn/s/?v=1&action=socket&url=ws%3A%2F%2F192.168.1.133%3A10001%2F%3Ftid%3Dabc-123"
        );
    }

    #[test]
    fn pairing_deep_link_embeds_the_raw_ws_url_unencoded() {
        // Despite the reference docs suggesting `encodeURIComponent`,
        // real-device testing against the actual DG-LAB APP shows it
        // expects the literal ws:// URL after the marker -- encoding it
        // makes pairing fail.
        let raw = "ws://192.168.1.133:10002/abc-123";
        let link = pairing_deep_link(raw);
        assert_eq!(
            link,
            "https://www.dungeon-lab.com/app-download.php#DGLAB-SOCKET#ws://192.168.1.133:10002/abc-123"
        );
    }

    #[test]
    fn render_svg_produces_svg_markup_without_the_xml_prolog() {
        let svg = render_svg("ws://127.0.0.1:10002/abc-123").unwrap();
        assert!(svg.trim_start().starts_with("<svg"));
        assert!(!svg.contains("<?xml"));
    }
}

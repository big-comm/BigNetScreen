//! The page a receiver opens: a PIN prompt, then the picture.
//!
//! One self-contained HTML document, no external assets, so it works on a TV
//! browser that has never seen this network before. The texts come from the
//! application so they follow its language.

/// Everything the page says, in the application's language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageText {
    /// The window title and heading, e.g. the sharing computer's name.
    pub title: String,
    /// "Enter the PIN shown on the computer".
    pub prompt: String,
    /// The button that submits the PIN.
    pub join: String,
    /// Shown when the PIN is wrong.
    pub wrong_pin: String,
    /// Shown after too many wrong PINs.
    pub locked: String,
    /// While the connection is being set up.
    pub connecting: String,
    /// When the connection could not be made.
    pub failed: String,
    /// When the sharing computer stopped.
    pub ended: String,
    /// Hint under the picture.
    pub fullscreen_hint: String,
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Renders the page. Every text is escaped; nothing from the network goes in.
pub fn render(text: &PageText) -> String {
    TEMPLATE
        .replace("{{title}}", &escape(&text.title))
        .replace("{{prompt}}", &escape(&text.prompt))
        .replace("{{join}}", &escape(&text.join))
        .replace("{{wrong_pin}}", &escape(&text.wrong_pin))
        .replace("{{locked}}", &escape(&text.locked))
        .replace("{{connecting}}", &escape(&text.connecting))
        .replace("{{failed}}", &escape(&text.failed))
        .replace("{{ended}}", &escape(&text.ended))
        .replace("{{fullscreen_hint}}", &escape(&text.fullscreen_hint))
}

const TEMPLATE: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{title}}</title>
<style>
  html, body { margin: 0; height: 100%; background: #000; color: #eee; font-family: sans-serif; }
  #gate { position: fixed; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 1.2em; padding: 1em; text-align: center; }
  h1 { font-size: 1.6em; font-weight: 600; margin: 0; }
  p { font-size: 1.15em; margin: 0; max-width: 28em; }
  #pin { font-size: 2.6em; letter-spacing: 0.4em; text-align: center; width: 6.5em; padding: 0.2em 0 0.2em 0.4em; border: 2px solid #555; border-radius: 0.3em; background: #111; color: #fff; }
  #pin:focus { outline: none; border-color: #4a90e2; }
  button { font-size: 1.3em; padding: 0.55em 1.6em; border: none; border-radius: 0.4em; background: #3584e4; color: #fff; }
  button:focus { outline: 3px solid #fff; }
  #msg { min-height: 1.5em; color: #f66; }
  #stage { display: none; position: fixed; inset: 0; background: #000; }
  video { width: 100%; height: 100%; object-fit: contain; background: #000; }
  #status { position: fixed; left: 0; right: 0; bottom: 6%; text-align: center; font-size: 1.3em; text-shadow: 0 0 8px #000; pointer-events: none; }
  #hint { position: fixed; left: 0; right: 0; bottom: 2%; text-align: center; color: #888; font-size: 0.95em; pointer-events: none; transition: opacity 1s; }
</style>
</head>
<body>
<div id="gate">
  <h1>{{title}}</h1>
  <p>{{prompt}}</p>
  <form id="form" autocomplete="off">
    <input id="pin" type="tel" inputmode="numeric" pattern="[0-9]*" maxlength="4" autofocus>
    <div style="height:1em"></div>
    <button id="join" type="submit">{{join}}</button>
  </form>
  <div id="msg"></div>
</div>
<div id="stage">
  <video id="video" autoplay playsinline></video>
  <div id="status">{{connecting}}</div>
  <div id="hint">{{fullscreen_hint}}</div>
</div>
<script>
(function () {
  var gate = document.getElementById('gate');
  var stage = document.getElementById('stage');
  var msg = document.getElementById('msg');
  var status = document.getElementById('status');
  var hint = document.getElementById('hint');
  var video = document.getElementById('video');
  var pin = document.getElementById('pin');
  var form = document.getElementById('form');
  var resource = null;
  var token = null;

  function showStatus(text) { status.textContent = text; status.style.display = text ? 'block' : 'none'; }

  form.addEventListener('submit', function (ev) {
    ev.preventDefault();
    msg.textContent = '';
    var code = pin.value.replace(/\D/g, '');
    if (code.length !== 4) { pin.focus(); return; }
    fetch('/pin', { method: 'POST', headers: { 'Content-Type': 'text/plain' }, body: code })
      .then(function (r) {
        if (r.status === 200) { return r.text(); }
        if (r.status === 429) { throw new Error('{{locked}}'); }
        throw new Error('{{wrong_pin}}');
      })
      .then(function (t) { token = t.trim(); gate.style.display = 'none'; stage.style.display = 'block'; start(); })
      .catch(function (e) { msg.textContent = e.message; pin.value = ''; pin.focus(); });
  });

  function lowLatency(pc) {
    pc.getReceivers().forEach(function (r) {
      // Ask the browser to hold as little as it can; a screen is not a film.
      try { if ('playoutDelayHint' in r) { r.playoutDelayHint = 0; } } catch (e) {}
      try { if ('jitterBufferTarget' in r) { r.jitterBufferTarget = 0; } } catch (e) {}
    });
  }

  function start() {
    showStatus('{{connecting}}');
    var pc = new RTCPeerConnection({ iceServers: [] });
    var stream = new MediaStream();
    video.srcObject = stream;
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });
    pc.ontrack = function (ev) { stream.addTrack(ev.track); lowLatency(pc); };
    pc.onconnectionstatechange = function () {
      var s = pc.connectionState;
      if (s === 'connected') { showStatus(''); setTimeout(function () { hint.style.opacity = 0; }, 6000); }
      else if (s === 'failed') { showStatus('{{failed}}'); }
      else if (s === 'disconnected' || s === 'closed') { showStatus('{{ended}}'); }
    };
    pc.createOffer().then(function (offer) {
      return pc.setLocalDescription(offer);
    }).then(function () {
      // Gather host candidates first so a single POST carries them all; TVs
      // do not always implement trickle ICE.
      return new Promise(function (resolve) {
        if (pc.iceGatheringState === 'complete') { resolve(); return; }
        var done = false;
        function finish() { if (!done) { done = true; resolve(); } }
        pc.addEventListener('icegatheringstatechange', function () { if (pc.iceGatheringState === 'complete') { finish(); } });
        setTimeout(finish, 1500);
      });
    }).then(function () {
      return fetch('/whep?token=' + encodeURIComponent(token), {
        method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp
      });
    }).then(function (r) {
      if (r.status !== 201) { throw new Error('{{failed}}'); }
      resource = r.headers.get('Location');
      return r.text();
    }).then(function (answer) {
      return pc.setRemoteDescription({ type: 'answer', sdp: answer });
    }).catch(function (e) { showStatus(e.message || '{{failed}}'); });

    video.addEventListener('click', function () {
      var el = document.documentElement;
      if (document.fullscreenElement) { document.exitFullscreen && document.exitFullscreen(); }
      else if (el.requestFullscreen) { el.requestFullscreen().catch(function () {}); }
    });
    // Tell the computer we left, so it stops counting us at once instead of
    // waiting for the connection to time out. Both events, because browsers
    // disagree on which one fires when a tab or a TV app closes.
    var left = false;
    function leave() {
      if (left) { return; }
      left = true;
      if (resource) { try { fetch(resource, { method: 'DELETE', keepalive: true }); } catch (e) {} }
      pc.close();
    }
    window.addEventListener('pagehide', leave);
    window.addEventListener('beforeunload', leave);
  }
})();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> PageText {
        PageText {
            title: "Tales <b>PC</b>".into(),
            prompt: "Enter the PIN".into(),
            join: "Join".into(),
            wrong_pin: "Wrong PIN".into(),
            locked: "Too many tries".into(),
            connecting: "Connecting…".into(),
            failed: "Could not connect".into(),
            ended: "Sharing ended".into(),
            fullscreen_hint: "Tap for full screen".into(),
        }
    }

    #[test]
    fn texts_are_escaped_and_no_placeholder_is_left() {
        let html = render(&text());
        assert!(html.contains("Tales &lt;b&gt;PC&lt;/b&gt;"));
        assert!(!html.contains("{{"), "unfilled placeholder in the page");
        assert!(html.contains("/whep?token="));
        assert!(html.contains("fetch('/pin'"));
    }
}

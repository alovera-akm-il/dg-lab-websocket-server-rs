(() => {
  const $ = (id) => document.getElementById(id);
  const strengthAEl = $('strength-a');
  const strengthBEl = $('strength-b');
  const strengthAInlineEl = $('strength-a-inline');
  const strengthBInlineEl = $('strength-b-inline');
  const strengthACapEl = $('strength-a-cap');
  const strengthBCapEl = $('strength-b-cap');
  const sliderAEl = $('slider-a');
  const sliderBEl = $('slider-b');
  const buttonActionEl = $('button-action');
  const activeDeviceEl = $('active-device');
  const batteryRowEl = $('battery-row');
  const channelAStatusEl = $('channel-a-status');
  const channelBStatusEl = $('channel-b-status');
  const overheatNoteEl = $('overheat-note');
  const overheatNoteTextEl = $('overheat-note-text');
  const logEl = $('log');
  const logCountEl = $('log-count');
  const toast = $('toast');
  const webhookCurrentEl = $('webhook-current');
  const summaryDotEl = $('summary-dot');
  const summaryTextEl = $('summary-text');
  const reconnectIconEl = $('reconnect-icon');
  const infoPopoverEl = $('info-popover');

  const STATUS_LABELS = {
    connecting: 'Connecting to relay...',
    waiting_for_device: 'Waiting for device to pair',
    paired: 'Paired',
    disconnected: 'Disconnected',
  };

  const MAX_STRENGTH = 200;

  // Tracks the previous render's values so we only animate what actually
  // changed (a value flash on strength updates, an entrance animation on
  // newly-appended log lines) instead of replaying motion on every SSE tick.
  let prevStrengthA = undefined;
  let prevStrengthB = undefined;
  let prevLogLength = 0;

  function showToast(message) {
    toast.textContent = message;
    toast.classList.add('show');
    setTimeout(() => toast.classList.remove('show'), 3000);
  }

  function flash(el) {
    el.style.color = 'var(--accent)';
    setTimeout(() => { el.style.color = ''; }, 450);
  }

  // Tactile confirmation for controls that actually cause the physical
  // device to do something -- strength changes, triggering/stopping a
  // waveform, playlist transport -- not for settings/UI-only actions
  // (limits, webhook, mode/preset pickers). Android Chrome supports the
  // Vibration API; iOS Safari doesn't implement it at all, so this is a
  // no-op there rather than an error.
  function vibrate(ms) {
    if (navigator.vibrate) {
      try { navigator.vibrate(ms); } catch (e) { /* ignore */ }
    }
  }

  // -- tap-to-open info popover ---------------------------------------
  //
  // Explains an ambiguous reading (e.g. "battery: 0" that might just mean
  // "not reported") without relying on a `title` attribute's hover-only
  // tooltip, which never fires on a touchscreen -- and this panel is
  // routinely opened from the very phone that's pairing. Delegated on
  // `document` (rather than wired per-button) since the elements that
  // trigger it, like the battery pill, get rebuilt from scratch on every
  // SSE render.
  let openInfoTrigger = null;

  function hideInfoPopover() {
    infoPopoverEl.hidden = true;
    openInfoTrigger = null;
  }

  function showInfoPopover(trigger) {
    infoPopoverEl.textContent = trigger.dataset.infoText || '';
    infoPopoverEl.hidden = false;
    const rect = trigger.getBoundingClientRect();
    const maxLeft = window.innerWidth - infoPopoverEl.offsetWidth - 12;
    const left = Math.max(12, Math.min(rect.left, Math.max(12, maxLeft)));
    infoPopoverEl.style.top = `${rect.bottom + 6}px`;
    infoPopoverEl.style.left = `${left}px`;
    openInfoTrigger = trigger;
  }

  document.addEventListener('click', (e) => {
    const btn = e.target.closest('.info-btn');
    if (btn) {
      e.stopPropagation();
      if (openInfoTrigger === btn) hideInfoPopover(); else showInfoPopover(btn);
      return;
    }
    if (openInfoTrigger && !infoPopoverEl.contains(e.target)) hideInfoPopover();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') hideInfoPopover();
  });

  function renderLeg(suffix, status, active, isPaired) {
    $(`status-dot-${suffix}`).className = 'dot ' + status;
    $(`status-label-${suffix}`).textContent = STATUS_LABELS[status] || status;
    $(`active-badge-${suffix}`).hidden = !active;
    $(`qr-box-${suffix}`).classList.toggle(`qr-pulse-${suffix}`, !isPaired);
  }

  function renderQr(suffix, svg, pairUrl) {
    const box = $(`qr-box-${suffix}`);
    const placeholder = box.querySelector('.qr-placeholder');
    if (svg) {
      box.innerHTML = svg;
    } else if (!placeholder) {
      box.innerHTML = '<span class="qr-placeholder">Waiting for controller id&hellip;</span>';
    }
    $(`pair-link-${suffix}`).textContent = pairUrl || '';
  }

  // -- device health: battery, per-channel output status, overheat -------
  //
  // All V4/Coyote-only (see docs/api.md) -- blank on V3, same "protocol
  // reports what the other doesn't" pattern already used for strength's
  // soft limit.
  function batteryLevelClass(pct) {
    if (pct <= 20) return 'critical';
    if (pct <= 50) return 'low';
    return 'ok';
  }

  // A reading of exactly 0 is indistinguishable, on the wire, from a
  // device/APP combination that just doesn't populate `power` at all --
  // we've confirmed this happens in practice (a real device sitting at a
  // healthy charge, per its own APP, still reports `power: 0` over V4
  // indefinitely). Rather than presenting that with the same confidence
  // as a real reading (and risking it read as "critically low"), flag it
  // as unreported instead of guessing either way.
  function renderBattery(pct) {
    if (pct == null) {
      batteryRowEl.textContent = '-';
      return;
    }
    if (pct === 0) {
      batteryRowEl.innerHTML =
        '<span class="battery-pill unreported">' +
        '<svg class="battery-icon" width="20" height="11" viewBox="0 0 22 12">' +
        '<rect class="battery-outline" x="1" y="1" width="17" height="10" rx="2"></rect>' +
        '<rect class="battery-nub" x="19.5" y="4" width="2" height="4" rx="1"></rect>' +
        '</svg>' +
        '<span class="mono">not reported</span>' +
        '<button type="button" class="info-btn" aria-label="Why is this uncertain?" ' +
        'data-info-text="This device/APP is not sending a real battery level over V4 — 0% here does not necessarily mean the battery is actually empty.">' +
        '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>' +
        '</button>' +
        '</span>';
      return;
    }
    const fillWidth = Math.max(0, Math.min(14, (pct / 100) * 14));
    batteryRowEl.innerHTML =
      `<span class="battery-pill ${batteryLevelClass(pct)}">` +
      '<svg class="battery-icon" width="20" height="11" viewBox="0 0 22 12">' +
      '<rect class="battery-outline" x="1" y="1" width="17" height="10" rx="2"></rect>' +
      '<rect class="battery-nub" x="19.5" y="4" width="2" height="4" rx="1"></rect>' +
      `<rect class="battery-fill" x="3" y="3" width="${fillWidth.toFixed(1)}" height="6" rx="1"></rect>` +
      '</svg>' +
      `<span class="mono">${pct}%</span>` +
      '</span>';
  }

  function channelStatusClass(code) {
    if (code === 2) return 'ok'; // normal
    if (code === 3) return 'err'; // damaged
    return 'warn'; // no output / open circuit / masked
  }

  function renderChannelStatus(el, code, label) {
    if (code == null) {
      el.hidden = true;
      return;
    }
    el.hidden = false;
    el.className = `channel-status ${channelStatusClass(code)}`;
    el.textContent = label || code;
  }

  function renderOverheat(state) {
    const messages = [];
    if (state.channelAOverheat) {
      messages.push(`Channel A is in overheat cooldown (${state.channelAOverheatPercent ?? '?'}%)`);
    }
    if (state.channelBOverheat) {
      messages.push(`Channel B is in overheat cooldown (${state.channelBOverheatPercent ?? '?'}%)`);
    }
    overheatNoteEl.hidden = messages.length === 0;
    if (messages.length) {
      overheatNoteTextEl.textContent = `${messages.join('; ')} — strength increases may not take effect until it clears.`;
    }
  }

  // -- live strength sliders --------------------------------------------
  //
  // Dragging the slider sends /api/strength {op:"set"} itself -- there's
  // no separate "Set" button. Sending on every native `input` tick would
  // flood the relay (and the physical device) with a command per pixel of
  // drag, so sends are throttled to at most one every THROTTLE_MS while
  // actively dragging, with a final guaranteed send on release (`change`)
  // so the exact released value always reaches the device even if it
  // didn't land on a throttle tick.
  const THROTTLE_MS = 120;

  function makeSliderController(channel, sliderEl, valueEl, capEl) {
    let dragging = false;
    let lastSentAt = 0;
    let pendingTimer = null;
    let currentLimit = null; // the operator-configured upper limit, or null

    function setFill(value, max) {
      const pct = max > 0 ? Math.max(0, Math.min(100, (value / max) * 100)) : 0;
      sliderEl.style.setProperty('--pct', `${pct}%`);
    }

    function setDisplay(value) {
      valueEl.firstChild.textContent = value == null ? '-' : String(value);
      capEl.textContent = currentLimit != null ? `cap ${currentLimit}` : '';
    }

    function send(value) {
      lastSentAt = Date.now();
      postJson('/api/strength', { channel, op: 'set', value });
    }

    function scheduleSend(value) {
      const elapsed = Date.now() - lastSentAt;
      if (elapsed >= THROTTLE_MS) {
        send(value);
      } else if (!pendingTimer) {
        pendingTimer = setTimeout(() => {
          pendingTimer = null;
          send(Number(sliderEl.value));
        }, THROTTLE_MS - elapsed);
      }
    }

    sliderEl.addEventListener('pointerdown', () => { dragging = true; });
    sliderEl.addEventListener('input', () => {
      const value = Number(sliderEl.value);
      setDisplay(value);
      setFill(value, Number(sliderEl.max));
      scheduleSend(value);
    });
    sliderEl.addEventListener('change', () => {
      if (pendingTimer) { clearTimeout(pendingTimer); pendingTimer = null; }
      send(Number(sliderEl.value));
      dragging = false;
    });
    // Keyboard-driven changes (arrow keys) don't fire pointerdown; `change`
    // still fires once the key is released, which is enough to release
    // the drag lock below without needing a separate keyup handler.
    sliderEl.addEventListener('blur', () => { dragging = false; });

    return {
      isDragging: () => dragging,
      sync(value, limit) {
        currentLimit = limit;
        const max = limit != null ? Math.max(limit, value ?? 0) : MAX_STRENGTH;
        sliderEl.max = String(max);
        sliderEl.value = String(value ?? 0);
        setFill(value ?? 0, max);
        setDisplay(value);
      },
    };
  }

  const sliderA = makeSliderController('A', sliderAEl, strengthAInlineEl, strengthACapEl);
  const sliderB = makeSliderController('B', sliderBEl, strengthBInlineEl, strengthBCapEl);

  // -- strength ramps: a programmatic curve the panel drives on a 1s
  // tick, in place of individual manual +/-/Set calls ------------------

  function formatEta(seconds) {
    const m = Math.floor(seconds / 60);
    const s = seconds % 60;
    return m > 0 ? `${m}:${String(s).padStart(2, '0')} left` : `${s}s left`;
  }

  function rampSummary(ramp) {
    if (ramp.profile === 'linear') return `${ramp.from} → ${ramp.to} over ${ramp.overSeconds}s`;
    if (ramp.profile === 'random-walk') return `${ramp.base} ± ${ramp.variance}, every ${ramp.stepSeconds}s`;
    if (ramp.profile === 'hold') return `holding ${ramp.value} for ${ramp.durationSeconds}s`;
    return '';
  }

  function makeRampController(suffix, channel) {
    const toggle = $(`ramp-profile-toggle-${suffix}`);
    const fieldsByProfile = {
      linear: $(`ramp-fields-linear-${suffix}`),
      'random-walk': $(`ramp-fields-random-walk-${suffix}`),
      hold: $(`ramp-fields-hold-${suffix}`),
    };
    let selectedProfile = 'linear';

    toggle.querySelectorAll('.mode-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        selectedProfile = btn.dataset.rampProfile;
        toggle.querySelectorAll('.mode-btn').forEach((b) => b.classList.toggle('active', b === btn));
        Object.entries(fieldsByProfile).forEach(([profile, el]) => {
          el.hidden = profile !== selectedProfile;
        });
      });
    });

    function intVal(id, fallback) {
      const v = parseInt($(id).value, 10);
      return Number.isFinite(v) ? v : fallback;
    }

    function buildBody() {
      if (selectedProfile === 'linear') {
        return {
          channel,
          profile: 'linear',
          from: intVal(`ramp-from-${suffix}`, 0),
          to: intVal(`ramp-to-${suffix}`, 0),
          overSeconds: intVal(`ramp-over-${suffix}`, 1),
        };
      }
      if (selectedProfile === 'random-walk') {
        return {
          channel,
          profile: 'random-walk',
          base: intVal(`ramp-base-${suffix}`, 0),
          variance: intVal(`ramp-variance-${suffix}`, 0),
          stepSeconds: intVal(`ramp-step-${suffix}`, 1),
          durationSeconds: intVal(`ramp-rw-duration-${suffix}`, 1),
        };
      }
      return {
        channel,
        profile: 'hold',
        value: intVal(`ramp-hold-value-${suffix}`, 0),
        durationSeconds: intVal(`ramp-hold-duration-${suffix}`, 1),
      };
    }

    $(`ramp-start-${suffix}`).addEventListener('click', () => {
      vibrate(20);
      postJson('/api/ramp', buildBody());
    });
    $(`ramp-cancel-${suffix}`).addEventListener('click', () => {
      vibrate(30);
      postJson('/api/ramp/stop', { channel });
    });

    const configEl = $(`ramp-config-${suffix}`);
    const activeEl = $(`ramp-active-${suffix}`);
    const currentEl = $(`ramp-current-${suffix}`);
    const targetEl = $(`ramp-target-${suffix}`);
    const etaEl = $(`ramp-eta-${suffix}`);
    const progressEl = $(`ramp-progress-${suffix}`);
    const profileTagEl = $(`ramp-active-profile-${suffix}`);
    const summaryEl = $(`ramp-active-summary-${suffix}`);

    return {
      render(ramp) {
        configEl.hidden = ramp != null;
        activeEl.hidden = ramp == null;
        if (!ramp) return;
        currentEl.textContent = ramp.current;
        targetEl.textContent = ramp.target != null ? `${ramp.target} target` : '';
        etaEl.textContent = formatEta(ramp.remainingSeconds);
        profileTagEl.textContent = ramp.profile;
        summaryEl.textContent = rampSummary(ramp);
        const total = ramp.profile === 'linear' ? ramp.overSeconds : ramp.durationSeconds;
        const pct = total > 0 ? Math.max(0, Math.min(100, 100 - (ramp.remainingSeconds / total) * 100)) : 0;
        progressEl.style.width = `${pct}%`;
      },
    };
  }

  const rampA = makeRampController('a', 'A');
  const rampB = makeRampController('b', 'B');

  function render(state) {
    const v4Paired = state.v4Status === 'paired';
    const v3Paired = state.status === 'paired';
    renderLeg('v4', state.v4Status, state.activeProtocol === 'v4', v4Paired);
    renderLeg('v3', state.status, state.activeProtocol === 'v3', v3Paired);
    renderQr('v4', state.qrSvgV4, state.pairUrlV4);
    renderQr('v3', state.qrSvg, state.pairUrl);

    let summaryText = 'No device paired';
    if (state.activeProtocol === 'v3') summaryText = `V3: ${state.deviceId || '-'}`;
    else if (state.activeProtocol === 'v4') summaryText = `V4: ${state.v4DeviceName || state.v4DeviceId || '-'}`;
    summaryTextEl.textContent = summaryText;
    summaryDotEl.className = 'dot ' + (state.activeProtocol ? 'paired' : 'waiting_for_device');
    activeDeviceEl.textContent = summaryText === 'No device paired' ? 'none' : summaryText;

    const hasActiveDevice = state.activeProtocol != null;
    $('strength-controls').disabled = !hasActiveDevice;
    $('pulse-controls').disabled = !hasActiveDevice;
    $('controls-disabled-note').hidden = hasActiveDevice;

    const strengthAText = state.strengthA != null
      ? `${state.strengthA}${state.softLimitA != null ? ` (limit ${state.softLimitA})` : ''}`
      : '-';
    const strengthBText = state.strengthB != null
      ? `${state.strengthB}${state.softLimitB != null ? ` (limit ${state.softLimitB})` : ''}`
      : '-';
    strengthAEl.textContent = strengthAText;
    strengthBEl.textContent = strengthBText;
    renderBattery(state.battery);
    renderChannelStatus(channelAStatusEl, state.channelAStatus, state.channelAStatusLabel);
    renderChannelStatus(channelBStatusEl, state.channelBStatus, state.channelBStatusLabel);
    renderOverheat(state);
    if (prevStrengthA !== undefined && state.strengthA !== prevStrengthA && !sliderA.isDragging()) flash(strengthAInlineEl);
    if (prevStrengthB !== undefined && state.strengthB !== prevStrengthB && !sliderB.isDragging()) flash(strengthBInlineEl);
    prevStrengthA = state.strengthA;
    prevStrengthB = state.strengthB;

    // While the operator is actively dragging a slider, the local drag
    // position is the source of truth for that channel -- an in-flight
    // SSE snapshot echoing our own (possibly not-yet-applied) throttled
    // send would otherwise yank the thumb back mid-drag.
    if (!sliderA.isDragging()) sliderA.sync(state.strengthA, state.limitA);
    if (!sliderB.isDragging()) sliderB.sync(state.strengthB, state.limitB);
    rampA.render(state.rampA);
    rampB.render(state.rampB);

    buttonActionEl.textContent = state.lastButtonAction != null ? state.lastButtonAction : '-';
    $('limit-a-current').textContent = state.limitA != null ? `current: ${state.limitA}` : 'no limit';
    $('limit-b-current').textContent = state.limitB != null ? `current: ${state.limitB}` : 'no limit';
    webhookCurrentEl.textContent = state.webhookUrl ? `Current: ${state.webhookUrl}` : 'No webhook configured.';

    const log = state.log || [];
    const wasAtBottom = logEl.scrollTop + logEl.clientHeight >= logEl.scrollHeight - 4;
    logEl.textContent = '';
    log.forEach((line, i) => {
      const div = document.createElement('div');
      div.className = 'log-line' + (i >= prevLogLength ? ' log-line-new' : '');
      div.textContent = line;
      logEl.appendChild(div);
    });
    prevLogLength = log.length;
    logCountEl.textContent = `${log.length} events`;
    if (wasAtBottom) logEl.scrollTop = logEl.scrollHeight;

    modeToggleA.autoSelect(state.playlistA);
    modeToggleB.autoSelect(state.playlistB);
    playlistA.render(state.playlistA);
    playlistB.render(state.playlistB);
  }

  const events = new EventSource('/events');
  events.onmessage = (ev) => {
    try { render(JSON.parse(ev.data)); } catch (e) { /* ignore malformed frame */ }
  };
  events.onerror = () => {
    $('status-label-v3').textContent = 'Lost connection to panel server, retrying...';
    $('status-label-v4').textContent = 'Lost connection to panel server, retrying...';
  };

  async function postJson(url, body) {
    const res = await fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body || {}),
    });
    if (!res.ok) {
      let message = res.statusText;
      try { message = (await res.json()).error || message; } catch (e) { /* ignore */ }
      showToast(message);
    }
  }

  async function deleteJson(url) {
    const res = await fetch(url, { method: 'DELETE' });
    if (!res.ok) {
      let message = res.statusText;
      try { message = (await res.json()).error || message; } catch (e) { /* ignore */ }
      showToast(message);
    }
  }

  document.querySelectorAll('[data-strength]').forEach((btn) => {
    const payload = JSON.parse(btn.getAttribute('data-strength'));
    btn.addEventListener('click', () => { vibrate(15); postJson('/api/strength', payload); });
  });

  document.querySelectorAll('[data-clear]').forEach((btn) => {
    const channel = btn.getAttribute('data-clear');
    btn.addEventListener('click', () => { vibrate(30); postJson('/api/clear', { channel }); });
  });

  document.querySelectorAll('[data-limit-set]').forEach((btn) => {
    const channel = btn.getAttribute('data-limit-set');
    const inputId = btn.getAttribute('data-value-input');
    btn.addEventListener('click', () => {
      const raw = $(inputId).value;
      if (raw === '') { showToast('enter a limit first'); return; }
      postJson('/api/limit', { channel, value: parseInt(raw, 10) || 0 });
    });
  });

  document.querySelectorAll('[data-limit-clear]').forEach((btn) => {
    const channel = btn.getAttribute('data-limit-clear');
    btn.addEventListener('click', () => postJson('/api/limit', { channel, value: null }));
  });

  // -- tap-to-copy pairing links -----------------------------------------
  //
  // `navigator.clipboard` needs a secure context (https, or localhost) --
  // but this panel is normally opened over plain LAN http (see the
  // README's pairing-over-WiFi section), where it's simply undefined. The
  // `execCommand('copy')` fallback is deprecated but still works from
  // plain http origins in every browser that matters here.
  async function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch (e) { /* fall through to the fallback below */ }
    }
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.focus();
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
    document.body.removeChild(ta);
    return ok;
  }

  function wireCopyButton(suffix) {
    const btn = $(`copy-link-${suffix}`);
    btn.addEventListener('click', async () => {
      const text = $(`pair-link-${suffix}`).textContent;
      if (!text) { showToast('no pairing link yet'); return; }
      const ok = await copyText(text);
      showToast(ok ? 'Pairing link copied' : 'Could not copy — long-press to select the link');
      if (ok) {
        btn.classList.add('copied');
        setTimeout(() => btn.classList.remove('copied'), 1200);
      }
    });
  }
  wireCopyButton('v4');
  wireCopyButton('v3');

  $('reconnect-btn').addEventListener('click', () => {
    postJson('/api/reconnect');
    reconnectIconEl.classList.remove('spin-once');
    // Force a reflow so re-adding the class restarts the animation even if
    // it's still mid-way from a rapid double-click.
    void reconnectIconEl.offsetWidth;
    reconnectIconEl.classList.add('spin-once');
    setTimeout(() => reconnectIconEl.classList.remove('spin-once'), 620);
  });

  $('webhook-set-btn').addEventListener('click', () => {
    const url = $('webhook-url-input').value.trim();
    if (!url) { showToast('enter a webhook URL first'); return; }
    postJson('/api/webhook', { url });
  });

  $('webhook-clear-btn').addEventListener('click', () => {
    $('webhook-url-input').value = '';
    postJson('/api/webhook', { url: null });
  });

  // -- preset picker: each option shows a real sparkline of its intensity
  // data, not just a text label -------------------------------------------
  const FAMILY_LABELS = { coyote: 'Coyote 3.0', ovc: 'OVC (Opossum)' };
  const SVG_NS = 'http://www.w3.org/2000/svg';

  function parseWaveformFrames(waveformStr) {
    try {
      return JSON.parse(waveformStr.slice(waveformStr.indexOf(':') + 1));
    } catch (e) {
      return [];
    }
  }

  // Each 16-hex-char frame is 8 bytes: 4 bytes of an undocumented
  // frequency-ish value, then 4 bytes of intensity (0x00-0x64, i.e.
  // 0-100) -- see src/panel/presets.rs's module docs. All 4 intensity
  // bytes are identical in every bundled preset, so the first is enough.
  function frameIntensity(frame) {
    return parseInt(frame.slice(8, 10), 16) || 0;
  }

  function sparklineMarkup(waveformStr) {
    const frames = parseWaveformFrames(waveformStr);
    if (!frames.length) return '';
    const w = 44, h = 16, pad = 1.5;
    const n = frames.length;
    const points = frames.map((frame, i) => {
      const x = n > 1 ? (i / (n - 1)) * (w - pad * 2) + pad : w / 2;
      const y = h - pad - (frameIntensity(frame) / 100) * (h - pad * 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    }).join(' ');
    const area = `${pad},${h - pad} ${points} ${(w - pad).toFixed(1)},${(h - pad).toFixed(1)}`;
    return `<polygon points="${area}"></polygon><polyline points="${points}"></polyline>`;
  }

  function makeSparklineSvg(waveformStr) {
    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'preset-spark');
    svg.setAttribute('viewBox', '0 0 44 16');
    svg.setAttribute('preserveAspectRatio', 'none');
    svg.innerHTML = sparklineMarkup(waveformStr);
    return svg;
  }

  function makePresetPicker(suffix) {
    const trigger = $(`preset-trigger-${suffix}`);
    const label = $(`preset-trigger-label-${suffix}`);
    const spark = $(`preset-spark-${suffix}`);
    const menu = $(`preset-menu-${suffix}`);
    let selected = null;

    function close() {
      menu.classList.remove('open');
      menu.hidden = true;
      trigger.setAttribute('aria-expanded', 'false');
    }
    function open() {
      menu.hidden = false;
      menu.classList.add('open');
      trigger.setAttribute('aria-expanded', 'true');
    }
    function select(preset) {
      selected = preset;
      label.textContent = preset.label;
      spark.innerHTML = sparklineMarkup(preset.waveform);
      menu.querySelectorAll('.preset-item').forEach((el) => {
        el.classList.toggle('active', el.dataset.id === preset.id);
      });
      close();
    }

    trigger.addEventListener('click', (e) => {
      e.stopPropagation();
      if (menu.classList.contains('open')) close(); else open();
    });
    document.addEventListener('click', (e) => {
      if (menu.classList.contains('open') && !menu.contains(e.target) && e.target !== trigger) close();
    });
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') close();
    });

    return {
      populate(presets) {
        menu.innerHTML = '';
        const byFamily = new Map();
        for (const p of presets) {
          if (!byFamily.has(p.family)) byFamily.set(p.family, []);
          byFamily.get(p.family).push(p);
        }
        for (const [family, items] of byFamily.entries()) {
          const groupLabel = document.createElement('div');
          groupLabel.className = 'preset-group-label';
          groupLabel.textContent = FAMILY_LABELS[family] || family;
          menu.appendChild(groupLabel);
          for (const p of items) {
            const item = document.createElement('button');
            item.type = 'button';
            item.className = 'preset-item';
            item.dataset.id = p.id;
            item.appendChild(makeSparklineSvg(p.waveform));
            const itemLabel = document.createElement('span');
            itemLabel.textContent = p.label;
            item.appendChild(itemLabel);
            item.addEventListener('click', () => select(p));
            menu.appendChild(item);
          }
        }
        if (presets.length) select(presets[0]);
      },
      getSelected: () => selected,
    };
  }

  const presetPickerA = makePresetPicker('a');
  const presetPickerB = makePresetPicker('b');

  function wirePulseChannel(suffix, channel) {
    $(`trigger-preset-${suffix}`).addEventListener('click', () => {
      const picker = suffix === 'a' ? presetPickerA : presetPickerB;
      const chosen = picker.getSelected();
      if (!chosen) { showToast('presets not loaded yet'); return; }
      vibrate(20);
      postJson('/api/pulse', {
        channel,
        time: parseInt($(`pulse-time-${suffix}`).value, 10) || 3,
        waveform: chosen.waveform,
      });
    });

    $(`trigger-custom-${suffix}`).addEventListener('click', () => {
      const waveform = $(`custom-waveform-${suffix}`).value.trim();
      if (!waveform) { showToast('enter a waveform first'); return; }
      vibrate(20);
      postJson('/api/pulse', {
        channel,
        time: parseInt($(`pulse-time-${suffix}`).value, 10) || 3,
        waveform,
      });
    });
  }

  wirePulseChannel('a', 'A');
  wirePulseChannel('b', 'B');

  // -- Single/Playlist mode switch ----------------------------------------
  //
  // Which tab is showing is pure client-side UI state -- the server has
  // no notion of it, by design (both a channel's single-shot controls and
  // its playlist are always live regardless of which one is on screen).
  // Left alone, that means a fresh tab always opens on "Single" even
  // while a playlist is actively running -- indistinguishable from the
  // queue having vanished. So until the operator explicitly picks a tab
  // on *this* page load, `autoSelect` keeps it pointed at whichever one
  // reflects what's actually happening, e.g. so a second tab opened
  // while channel A's playlist is running shows it immediately instead
  // of defaulting to the (empty-looking) Single tab.

  function wireModeToggle(suffix) {
    const toggle = $(`mode-toggle-${suffix}`);
    const singleEl = $(`single-mode-${suffix}`);
    const playlistEl = $(`playlist-mode-${suffix}`);
    let userPicked = false;

    function setMode(mode) {
      toggle.querySelectorAll('.mode-btn').forEach((b) => b.classList.toggle('active', b.dataset.mode === mode));
      singleEl.hidden = mode !== 'single';
      playlistEl.hidden = mode !== 'playlist';
    }

    toggle.querySelectorAll('.mode-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        userPicked = true;
        setMode(btn.dataset.mode);
      });
    });

    return {
      autoSelect(playlist) {
        if (userPicked) return;
        const hasActivity = playlist.entries.length > 0 || playlist.phase !== 'stopped';
        setMode(hasActivity ? 'playlist' : 'single');
      },
    };
  }
  const modeToggleA = wireModeToggle('a');
  const modeToggleB = wireModeToggle('b');

  // -- pulse playlists ------------------------------------------------------
  //
  // Each queue entry's own duration is either fixed or randomized -- there
  // is no separate global "random duration" setting on the server, only
  // what's stored per entry at add time. The dice toggles here only decide
  // whether the *next* thing "+ Add"/"+ Add gap" creates uses a single
  // duration field or a min/max range; see
  // docs/channel-playlists-implementation.md.

  const DICE_ICON_SVG =
    '<svg class="mini-dice" width="10" height="10" viewBox="0 0 14 14" fill="none" stroke="currentColor" stroke-width="1.4">' +
    '<rect x="2" y="2" width="10" height="10" rx="2"/><circle cx="5" cy="5" r="0.8" fill="currentColor" stroke="none"/>' +
    '<circle cx="9" cy="9" r="0.8" fill="currentColor" stroke="none"/><circle cx="7" cy="7" r="0.8" fill="currentColor" stroke="none"/></svg>';
  const DRAG_HANDLE_SVG =
    '<svg class="drag-handle" width="10" height="16" viewBox="0 0 10 16"><circle cx="2" cy="3" r="1.3"/><circle cx="2" cy="8" r="1.3"/>' +
    '<circle cx="2" cy="13" r="1.3"/><circle cx="7" cy="3" r="1.3"/><circle cx="7" cy="8" r="1.3"/><circle cx="7" cy="13" r="1.3"/></svg>';
  const REMOVE_ICON_SVG =
    '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M18 6 6 18"/><path d="M6 6l12 12"/></svg>';
  const GAP_ICON_SVG =
    '<svg class="gap-icon" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">' +
    '<path d="M11 5 6 9H2v6h4l5 4V5z"/><line x1="23" y1="9" x2="17" y2="15"/><line x1="17" y1="9" x2="23" y2="15"/></svg>';
  const PLAY_ICON_SVG = '<path d="M6 4l14 8-14 8V4z"/>';
  const PAUSE_ICON_SVG = '<rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/>';

  function svgFragment(markup) {
    const span = document.createElement('span');
    span.innerHTML = markup;
    return span.firstChild;
  }

  // Toggles a "+ Add" row's duration field between one fixed-seconds input
  // and a min/max pair, and reports whichever shape is currently active.
  function wireDurationToggle(toggleId, fixedId, minId, sepId, maxId) {
    const toggle = $(toggleId);
    const fixed = $(fixedId);
    const min = $(minId);
    const sep = $(sepId);
    const max = $(maxId);
    let random = false;
    toggle.addEventListener('click', () => {
      random = !random;
      toggle.classList.toggle('active', random);
      fixed.hidden = random;
      min.hidden = !random;
      sep.hidden = !random;
      max.hidden = !random;
    });
    return {
      getSpec() {
        if (random) {
          return {
            mode: 'random',
            min: parseInt(min.value, 10) || 1,
            max: parseInt(max.value, 10) || 1,
          };
        }
        return { mode: 'fixed', seconds: parseInt(fixed.value, 10) || 1 };
      },
    };
  }

  function durationLabel(duration) {
    return duration.mode === 'random' ? `${duration.min}–${duration.max}s` : `${duration.seconds}s`;
  }

  function makePlaylistController(suffix) {
    const base = `/api/playlist/${suffix}`;
    const picker = makePresetPicker(`playlist-${suffix}`);
    const itemDuration = wireDurationToggle(
      `item-random-toggle-${suffix}`, `item-duration-${suffix}`,
      `item-duration-min-${suffix}`, `item-duration-sep-${suffix}`, `item-duration-max-${suffix}`,
    );
    const gapDuration = wireDurationToggle(
      `gap-random-toggle-${suffix}`, `gap-duration-${suffix}`,
      `gap-duration-min-${suffix}`, `gap-duration-sep-${suffix}`, `gap-duration-max-${suffix}`,
    );

    const queueEl = $(`playlist-queue-${suffix}`);
    const playPauseBtn = $(`playlist-playpause-${suffix}`);
    const playPauseIcon = $(`playlist-playpause-icon-${suffix}`);
    const shuffleBtn = $(`playlist-shuffle-${suffix}`);
    const loopBtn = $(`playlist-loop-${suffix}`);
    const summaryEl = $(`playlist-summary-${suffix}`);

    $(`add-item-${suffix}`).addEventListener('click', () => {
      const chosen = picker.getSelected();
      if (!chosen) { showToast('presets not loaded yet'); return; }
      // Send the preset's id, not its resolved waveform -- the server
      // resolves it lazily (same as a bare custom string would pass
      // through unresolved), which is also what lets the queue show the
      // preset's real name instead of a raw-data preview.
      postJson(`${base}/items`, { kind: 'pulse', waveform: chosen.id, duration: itemDuration.getSpec() });
    });

    $(`add-gap-${suffix}`).addEventListener('click', () => {
      postJson(`${base}/items`, { kind: 'gap', duration: gapDuration.getSpec() });
    });

    playPauseBtn.addEventListener('click', () => {
      vibrate(20);
      postJson(`${base}/${playPauseBtn.dataset.action || 'play'}`);
    });
    $(`playlist-stop-${suffix}`).addEventListener('click', () => { vibrate(30); postJson(`${base}/stop`); });

    shuffleBtn.addEventListener('click', () => {
      postJson(`${base}/settings`, {
        shuffle: !shuffleBtn.classList.contains('active'),
        loopPlayback: loopBtn.classList.contains('active'),
      });
    });
    loopBtn.addEventListener('click', () => {
      postJson(`${base}/settings`, {
        shuffle: shuffleBtn.classList.contains('active'),
        loopPlayback: !loopBtn.classList.contains('active'),
      });
    });

    // -- drag-to-reorder: native HTML5 DnD. On drop, the full new id
    // order is computed from the DOM and POSTed in one shot -- there's no
    // live-reordering animation while dragging, just the `.dragging`
    // opacity, kept deliberately simple for a first version.
    let dragId = null;
    function wireDrag(el, id) {
      el.draggable = true;
      el.addEventListener('dragstart', () => { dragId = id; el.classList.add('dragging'); });
      el.addEventListener('dragend', () => { el.classList.remove('dragging'); dragId = null; });
      el.addEventListener('dragover', (e) => e.preventDefault());
      el.addEventListener('drop', (e) => {
        e.preventDefault();
        if (!dragId || dragId === id) return;
        const order = Array.from(queueEl.children).map((c) => c.dataset.id).filter(Boolean).filter((x) => x !== dragId);
        order.splice(order.indexOf(id), 0, dragId);
        postJson(`${base}/reorder`, { order });
      });
    }

    function removeBtn(entryId) {
      const btn = document.createElement('button');
      btn.className = 'item-remove';
      btn.innerHTML = REMOVE_ICON_SVG;
      btn.addEventListener('click', () => deleteJson(`${base}/items/${entryId}`));
      return btn;
    }

    function renderItemRow(entry, isCurrent, playlist) {
      const row = document.createElement('div');
      row.className = 'playlist-item' + (isCurrent ? ' playing' : '');
      row.dataset.id = entry.id;
      if (isCurrent && playlist.remainingMs != null && playlist.currentDurationMs) {
        const bar = document.createElement('div');
        bar.className = 'playing-progress';
        const pct = 100 - Math.max(0, Math.min(100, (playlist.remainingMs / playlist.currentDurationMs) * 100));
        bar.style.width = `${pct}%`;
        row.appendChild(bar);
      }
      row.appendChild(svgFragment(DRAG_HANDLE_SVG));
      const spark = document.createElementNS(SVG_NS, 'svg');
      spark.setAttribute('class', 'item-spark');
      spark.setAttribute('viewBox', '0 0 44 16');
      spark.setAttribute('preserveAspectRatio', 'none');
      if (entry.waveformResolved) spark.innerHTML = sparklineMarkup(entry.waveformResolved);
      row.appendChild(spark);
      const name = document.createElement('span');
      name.className = 'item-name';
      name.textContent = entry.label;
      row.appendChild(name);
      if (isCurrent) {
        const tag = document.createElement('span');
        tag.className = 'playing-tag';
        tag.textContent = 'playing';
        row.appendChild(tag);
      }
      const dur = document.createElement('span');
      dur.className = 'item-duration mono';
      if (entry.duration.mode === 'random') dur.appendChild(svgFragment(DICE_ICON_SVG));
      const durText = document.createElement('span');
      durText.textContent = isCurrent && playlist.remainingMs != null && playlist.currentDurationMs
        ? `${Math.ceil(playlist.remainingMs / 1000)} / ${Math.ceil(playlist.currentDurationMs / 1000)}s`
        : durationLabel(entry.duration);
      dur.appendChild(durText);
      row.appendChild(dur);
      row.appendChild(removeBtn(entry.id));
      wireDrag(row, entry.id);
      return row;
    }

    function renderGapRow(entry, isCurrent, playlist) {
      const row = document.createElement('div');
      row.className = 'playlist-gap';
      row.dataset.id = entry.id;
      row.appendChild(svgFragment(DRAG_HANDLE_SVG));
      row.appendChild(svgFragment(GAP_ICON_SVG));
      const label = document.createElement('span');
      label.className = 'gap-label';
      label.textContent = 'Silent gap';
      row.appendChild(label);
      if (isCurrent && playlist.remainingMs != null && playlist.currentDurationMs) {
        const time = document.createElement('span');
        time.className = 'gap-time mono';
        time.textContent = `${Math.ceil(playlist.remainingMs / 1000)}s left`;
        row.appendChild(time);
        const track = document.createElement('div');
        track.className = 'gap-progress-track';
        const fill = document.createElement('div');
        fill.className = 'gap-progress-fill';
        const pct = 100 - Math.max(0, Math.min(100, (playlist.remainingMs / playlist.currentDurationMs) * 100));
        fill.style.width = `${pct}%`;
        track.appendChild(fill);
        row.appendChild(track);
      } else {
        const dur = document.createElement('span');
        dur.className = 'gap-duration mono';
        if (entry.duration.mode === 'random') dur.appendChild(svgFragment(DICE_ICON_SVG));
        const durText = document.createElement('span');
        durText.textContent = durationLabel(entry.duration);
        dur.appendChild(durText);
        row.appendChild(dur);
      }
      row.appendChild(removeBtn(entry.id));
      wireDrag(row, entry.id);
      return row;
    }

    function renderQueue(playlist) {
      queueEl.innerHTML = '';
      if (!playlist.entries.length) {
        const empty = document.createElement('div');
        empty.className = 'playlist-empty';
        empty.textContent = 'No items yet — add a preset or a gap above.';
        queueEl.appendChild(empty);
        return;
      }
      const isPlaying = playlist.phase === 'playing';
      for (const entry of playlist.entries) {
        const isCurrent = isPlaying && entry.id === playlist.currentId;
        queueEl.appendChild(
          entry.kind === 'gap' ? renderGapRow(entry, isCurrent, playlist) : renderItemRow(entry, isCurrent, playlist),
        );
      }
    }

    function renderControls(playlist) {
      const playing = playlist.phase === 'playing';
      playPauseIcon.innerHTML = playing ? PAUSE_ICON_SVG : PLAY_ICON_SVG;
      playPauseBtn.title = playing ? 'Pause' : 'Play';
      playPauseBtn.dataset.action = playing ? 'pause' : 'play';
      shuffleBtn.classList.toggle('active', playlist.shuffle);
      loopBtn.classList.toggle('active', playlist.loopPlayback);

      const total = playlist.entries.length;
      const current = playing ? playlist.entries.find((e) => e.id === playlist.currentId) : null;
      if (!total) {
        summaryEl.textContent = 'No items yet';
      } else if (current) {
        const index = playlist.entries.findIndex((e) => e.id === playlist.currentId) + 1;
        const remaining = playlist.remainingMs != null ? Math.ceil(playlist.remainingMs / 1000) : 0;
        summaryEl.textContent = current.kind === 'gap'
          ? `Silent gap · resumes in ${remaining}s`
          : `Item ${index} of ${total} · ${remaining}s left`;
      } else if (playlist.phase === 'paused') {
        summaryEl.textContent = 'Paused';
      } else {
        const upTo = playlist.entries.reduce(
          (sum, e) => sum + (e.duration.mode === 'random' ? e.duration.max : e.duration.seconds), 0,
        );
        summaryEl.textContent = `${total} item${total === 1 ? '' : 's'} · up to ${upTo}s total`;
      }
    }

    return {
      populate: picker.populate,
      render(playlist) {
        renderQueue(playlist);
        renderControls(playlist);
      },
    };
  }

  const playlistA = makePlaylistController('a');
  const playlistB = makePlaylistController('b');

  // -- playlist templates: save a channel's current queue under a name,
  // load it back into either channel later -----------------------------
  //
  // Templates are global (saved from one channel, loadable into either),
  // so both channels' pickers always show the same name list -- there's
  // no per-family grouping the way waveform presets have, just flat
  // names, so this is a simpler picker than `makePresetPicker`.

  function makeTemplatePicker(suffix) {
    const trigger = $(`template-trigger-${suffix}`);
    const label = $(`template-trigger-label-${suffix}`);
    const menu = $(`template-menu-${suffix}`);
    let selected = null;

    function close() {
      menu.classList.remove('open');
      menu.hidden = true;
      trigger.setAttribute('aria-expanded', 'false');
    }
    function open() {
      menu.hidden = false;
      menu.classList.add('open');
      trigger.setAttribute('aria-expanded', 'true');
    }
    function select(name) {
      selected = name;
      label.textContent = name;
      menu.querySelectorAll('.preset-item').forEach((el) => {
        el.classList.toggle('active', el.dataset.name === name);
      });
      close();
    }

    trigger.addEventListener('click', (e) => {
      e.stopPropagation();
      if (menu.classList.contains('open')) close(); else open();
    });
    document.addEventListener('click', (e) => {
      if (menu.classList.contains('open') && !menu.contains(e.target) && e.target !== trigger) close();
    });
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') close();
    });

    return {
      populate(names) {
        // Keep the current selection if it's still in the list (e.g.
        // after saving an unrelated template); otherwise fall back to
        // the first entry, or the empty state if there are none.
        const keepSelected = selected != null && names.includes(selected);
        menu.innerHTML = '';
        if (!names.length) {
          label.textContent = 'No templates yet';
          selected = null;
          return;
        }
        for (const name of names) {
          const item = document.createElement('button');
          item.type = 'button';
          item.className = 'preset-item';
          item.dataset.name = name;
          const itemLabel = document.createElement('span');
          itemLabel.textContent = name;
          item.appendChild(itemLabel);
          item.addEventListener('click', () => select(name));
          menu.appendChild(item);
        }
        select(keepSelected ? selected : names[0]);
      },
      getSelected: () => selected,
    };
  }

  const templatePickerA = makeTemplatePicker('a');
  const templatePickerB = makeTemplatePicker('b');

  function refreshTemplates() {
    fetch('/api/templates').then((r) => r.json()).then((names) => {
      templatePickerA.populate(names);
      templatePickerB.populate(names);
    });
  }

  function wireTemplateChannel(suffix, channel, picker) {
    $(`template-load-${suffix}`).addEventListener('click', () => {
      const name = picker.getSelected();
      if (!name) { showToast('no templates saved yet'); return; }
      const shuffleActive = $(`playlist-shuffle-${suffix}`).classList.contains('active');
      postJson(`/api/playlist/${suffix}/load-template`, { name, shuffle: shuffleActive });
    });

    $(`template-save-${suffix}`).addEventListener('click', () => {
      const input = $(`template-save-name-${suffix}`);
      const name = input.value.trim();
      if (!name) { showToast('enter a name first'); return; }
      fetch(`/api/templates/${encodeURIComponent(name)}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sourceChannel: channel }),
      }).then(async (res) => {
        if (!res.ok) {
          let message = res.statusText;
          try { message = (await res.json()).error || message; } catch (e) { /* ignore */ }
          throw new Error(message);
        }
        input.value = '';
        showToast(`Saved template "${name}"`);
        refreshTemplates();
      }).catch((e) => showToast(e.message || 'failed to save template'));
    });
  }

  wireTemplateChannel('a', 'A', templatePickerA);
  wireTemplateChannel('b', 'B', templatePickerB);
  refreshTemplates();

  fetch('/api/presets').then((r) => r.json()).then((presets) => {
    presetPickerA.populate(presets);
    presetPickerB.populate(presets);
    playlistA.populate(presets);
    playlistB.populate(presets);
  });
})();

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
  const logEl = $('log');
  const logCountEl = $('log-count');
  const toast = $('toast');
  const webhookCurrentEl = $('webhook-current');
  const summaryDotEl = $('summary-dot');
  const summaryTextEl = $('summary-text');
  const reconnectIconEl = $('reconnect-icon');

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

  document.querySelectorAll('[data-strength]').forEach((btn) => {
    const payload = JSON.parse(btn.getAttribute('data-strength'));
    btn.addEventListener('click', () => postJson('/api/strength', payload));
  });

  document.querySelectorAll('[data-clear]').forEach((btn) => {
    const channel = btn.getAttribute('data-clear');
    btn.addEventListener('click', () => postJson('/api/clear', { channel }));
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

  fetch('/api/presets').then((r) => r.json()).then((presets) => {
    presetPickerA.populate(presets);
    presetPickerB.populate(presets);
  });

  function wirePulseChannel(suffix, channel) {
    $(`trigger-preset-${suffix}`).addEventListener('click', () => {
      const picker = suffix === 'a' ? presetPickerA : presetPickerB;
      const chosen = picker.getSelected();
      if (!chosen) { showToast('presets not loaded yet'); return; }
      postJson('/api/pulse', {
        channel,
        time: parseInt($(`pulse-time-${suffix}`).value, 10) || 3,
        waveform: chosen.waveform,
      });
    });

    $(`trigger-custom-${suffix}`).addEventListener('click', () => {
      const waveform = $(`custom-waveform-${suffix}`).value.trim();
      if (!waveform) { showToast('enter a waveform first'); return; }
      postJson('/api/pulse', {
        channel,
        time: parseInt($(`pulse-time-${suffix}`).value, 10) || 3,
        waveform,
      });
    });
  }

  wirePulseChannel('a', 'A');
  wirePulseChannel('b', 'B');
})();

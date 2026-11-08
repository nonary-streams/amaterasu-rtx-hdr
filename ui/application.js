// The RTX HDR plugin owns this UI. The host supplies the app slot and scoped SDK.
window.AmaterasuExtensions.v1.register('rtx-hdr', {
  title: 'RTX HDR',
  mountApplication(container, sdk) {
    if (!sdk.features?.includes('ui.application-settings/1') ||
        sdk.settings?.provider !== 'amaterasu.rtx-hdr') {
      throw new Error('RTX HDR requires application settings support.');
    }
    const enableParameters = {
      global: ['rtx_hdr_enable', 'Enable RTX HDR', 'boolean', null, null],
      application: ['rtx_hdr_application_enable', 'Enable RTX HDR', 'boolean', null, null],
    };
    const pictureParameters = [
      ['rtx_hdr_peak_brightness', 'Peak brightness (nits)', 'integer', 400, 2000],
      ['rtx_hdr_middle_gray', 'Middle gray', 'integer', 10, 100],
      ['rtx_hdr_contrast', 'Contrast', 'integer', -100, 100],
      ['rtx_hdr_saturation', 'Saturation', 'integer', -100, 100],
    ];
    const advancedParameters = [
      ['rtx_hdr_fullscreen_only', 'Only convert fullscreen video', 'boolean', null, null],
      ['rtx_hdr_foreground_only', 'Only convert the foreground application', 'boolean', null, null],
      ['rtx_hdr_sdr_brightness', 'SDR fallback brightness', 'integer', 0, 100],
      ['rtx_hdr_debanding', 'Debanding', 'boolean', null, null],
    ];
    const fields = document.createElement('div');
    fields.className = 'rtx-hdr-groups';
    const groups = new Map();
    for (const [key, title] of [['activation', 'Application'], ['picture', 'Picture tuning'], ['advanced', 'Advanced']]) {
      const group = document.createElement(key === 'advanced' ? 'details' : 'section');
      group.className = key === 'advanced' ? 'rtx-hdr-group rtx-hdr-advanced' : 'rtx-hdr-group';
      const heading = document.createElement(key === 'advanced' ? 'summary' : 'h3');
      heading.textContent = title;
      const rows = document.createElement('div');
      rows.className = 'rtx-hdr-rows';
      group.append(heading, rows);
      groups.set(key, rows);
      fields.append(group);
    }
    const status = document.createElement('p');
    status.className = 'rtx-hdr-status';
    status.setAttribute('role', 'status');
    const help = document.createElement('p');
    help.className = 'rtx-hdr-help';
    help.textContent = 'Convert SDR games to HDR when your client requests HDR. SDR streams bypass the effect. Settings use your global values until you change them, and changes save automatically.';
    const timing = document.createElement('p');
    timing.className = 'rtx-hdr-timing';
    const surface = document.createElement('div');
    surface.className = 'rtx-hdr-settings';
    if (sdk.assets) void sdk.assets.stylesheet('application.css').catch(() => {
      if (!sdk.signal.aborted) timing.textContent += ' Appearance could not be loaded. Settings are still available.';
    });
    const controls = new Map();
    let documentState;
    let draft = {};
    let baseline = {};
    let generation = 0;
    let timer;
    let saving = false;
    let enableMode;
    const same = (a, b) => JSON.stringify(Object.entries(a).sort()) === JSON.stringify(Object.entries(b).sort());
    const dirty = () => !same(draft, baseline);
    const cancelTimer = () => { clearTimeout(timer); timer = undefined; };
    const schedule = () => {
      cancelTimer();
      if (!sdk.signal.aborted && dirty()) timer = setTimeout(() => { void save(); }, 200);
    };
    const changed = (id, value) => {
      if (!sdk.settings.canWrite || sdk.signal.aborted) return;
      draft[id] = value;
      generation++;
      sdk.setDirty(dirty());
      status.textContent = dirty() ? 'Changes waiting to save…' : 'No unsaved changes.';
      schedule();
    };
    const reflect = () => {
      syncEnableControl();
      for (const [id, item] of controls) {
        item.setValue(Object.hasOwn(draft, id) ? draft[id] : documentState.effective[id].value);
      }
    };
    const checkedDocument = (value) => {
      const parameters = [...Object.values(enableParameters), ...pictureParameters, ...advancedParameters];
      if (!value || !Number.isSafeInteger(value.revision) || value.revision < 0 ||
          !value.values || !value.effective || parameters.some(([id]) => !Object.hasOwn(value.effective, id)) ||
          !['global', 'application'].includes(value.effective.rtx_hdr_scope?.value)) {
        throw new Error('Invalid RTX HDR settings response.');
      }
      return value;
    };
    function addControl([id, label, type, minimum, maximum], group) {
      const value = Object.hasOwn(draft, id) ? draft[id] : documentState.effective[id].value;
      const options = { label, value, disabled: !sdk.settings.canWrite, onChange(value) { changed(id, value); } };
      const control = type === 'boolean' ? sdk.ui.toggle(options) : sdk.ui.slider({ ...options, minimum, maximum, step: 1 });
      const row = document.createElement('div');
      row.className = 'rtx-hdr-setting';
      row.setAttribute('role', 'group');
      row.setAttribute('aria-label', label);
      row.setAttribute('data-setting-id', id);
      row.append(control.element);
      group.append(row);
      controls.set(id, control);
    }
    function syncEnableControl() {
      const mode = documentState.effective.rtx_hdr_scope.value;
      if (mode === enableMode) return;
      if (enableMode) {
        const previousId = enableParameters[enableMode][0];
        controls.get(previousId).dispose();
        controls.delete(previousId);
      }
      groups.get('activation').replaceChildren();
      addControl(enableParameters[mode], groups.get('activation'));
      enableMode = mode;
      timing.textContent = mode === 'application'
        ? 'Enabling RTX HDR applies to the next stream. Picture tuning updates active RTX HDR streams.'
        : 'This application follows global RTX HDR mode. Changes can update active streams prepared for RTX HDR.';
    }
    async function save() {
      cancelTimer();
      if (saving || !documentState || !dirty() || !sdk.settings.canWrite || sdk.signal.aborted) return;
      saving = true;
      sdk.setBusy(true);
      const submitted = { ...draft };
      const submittedGeneration = generation;
      let saved = false;
      status.textContent = 'Saving RTX HDR settings…';
      try {
        const reply = await sdk.settings.update(documentState.revision, submitted);
        if (sdk.signal.aborted) return;
        if (!Number.isSafeInteger(reply.revision) || reply.revision <= documentState.revision) {
          throw new Error('The save could not be confirmed. Reload settings before retrying.');
        }
        documentState.revision = reply.revision;
        baseline = submitted;
        saved = true;
        sdk.setDirty(dirty());
        status.textContent = 'RTX HDR settings saved.';
        try {
          const refreshed = checkedDocument(await sdk.settings.read());
          if (sdk.signal.aborted) return;
          if (refreshed.revision !== documentState.revision) throw new Error('Settings changed elsewhere.');
          documentState = refreshed;
          syncEnableControl();
          if (generation === submittedGeneration) reflect();
        } catch (error) {
          if (!sdk.signal.aborted) status.textContent = `Saved, but inherited values could not be refreshed: ${error.message}`;
        }
      } catch (error) {
        if (!sdk.signal.aborted) {
          status.textContent = `Unable to save RTX HDR settings: ${error.message}`;
          sdk.setDirty(true);
        }
      } finally {
        saving = false;
        sdk.setBusy(false);
        if (saved && generation !== submittedGeneration) schedule();
      }
    }
    const saveButton = sdk.ui.button({ label: 'Save changes', onClick: save, disabled: !sdk.settings.canWrite });
    const footer = document.createElement('div');
    footer.className = 'rtx-hdr-footer';
    footer.append(status, saveButton.element);
    surface.append(help, timing, fields, footer);
    container.replaceChildren(surface);
    status.textContent = 'Loading RTX HDR settings…';
    void sdk.settings.read().then((value) => {
      if (sdk.signal.aborted) return;
      documentState = checkedDocument(value);
      draft = { ...documentState.values };
      baseline = { ...draft };
      syncEnableControl();
      for (const parameter of pictureParameters) addControl(parameter, groups.get('picture'));
      for (const parameter of advancedParameters) addControl(parameter, groups.get('advanced'));
      sdk.setDirty(false);
      status.textContent = sdk.settings.canWrite ? 'RTX HDR settings are up to date.' : 'RTX HDR settings are read-only.';
    }).catch((error) => {
      if (!sdk.signal.aborted) status.textContent = `Unable to load RTX HDR settings: ${error.message}`;
    });
    const dispose = () => {
      cancelTimer();
      for (const item of controls.values()) item.dispose();
      controls.clear();
      saveButton.dispose();
    };
    sdk.signal.addEventListener('abort', dispose, { once: true });
    return dispose;
  },
});

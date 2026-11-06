// The RTX HDR plugin owns this UI. The host supplies the app slot and scoped SDK.
window.AmaterasuExtensions.v1.register('rtx-hdr', {
  title: 'RTX HDR',
  mountApplication(container, sdk) {
    if (!sdk.features?.includes('ui.application-settings/1') ||
        sdk.settings?.provider !== 'amaterasu.rtx-hdr') {
      throw new Error('RTX HDR requires application settings support.');
    }
    const parameters = [
      ['rtx_hdr_enable', 'Enable RTX HDR', 'boolean', null, null],
      ['rtx_hdr_peak_brightness', 'Peak brightness (nits)', 'integer', 400, 2000],
      ['rtx_hdr_middle_gray', 'Middle gray', 'integer', 10, 100],
      ['rtx_hdr_contrast', 'Contrast', 'integer', -100, 100],
      ['rtx_hdr_saturation', 'Saturation', 'integer', -100, 100],
      ['rtx_hdr_fullscreen_only', 'Only convert fullscreen video', 'boolean', null, null],
      ['rtx_hdr_foreground_only', 'Only convert the foreground application', 'boolean', null, null],
      ['rtx_hdr_sdr_brightness', 'SDR fallback brightness', 'integer', 0, 100],
      ['rtx_hdr_debanding', 'Debanding', 'boolean', null, null],
    ];
    const fields = document.createElement('div');
    const status = document.createElement('p');
    status.setAttribute('role', 'status');
    const help = document.createElement('p');
    help.textContent = 'Changes save automatically. RTX HDR and tuning update active HDR streams after saving. Turning it off preserves SDR appearance within the HDR stream.';
    const controls = new Map();
    let documentState;
    let draft = {};
    let baseline = {};
    let generation = 0;
    let timer;
    let saving = false;
    const same = (a, b) => JSON.stringify(Object.entries(a).sort()) === JSON.stringify(Object.entries(b).sort());
    const dirty = () => !same(draft, baseline);
    const cancelTimer = () => { clearTimeout(timer); timer = undefined; };
    const schedule = () => {
      cancelTimer();
      if (!sdk.signal.aborted && dirty()) timer = setTimeout(() => { void save(); }, 200);
    };
    const changed = (id, value, inherit) => {
      if (!sdk.settings.canWrite || sdk.signal.aborted) return;
      if (inherit) delete draft[id];
      else draft[id] = value;
      generation++;
      sdk.setDirty(dirty());
      status.textContent = dirty() ? 'Changes waiting to save…' : 'No unsaved changes.';
      schedule();
    };
    const reflect = () => {
      for (const [id, item] of controls) {
        const inherited = !Object.hasOwn(draft, id);
        item.inherit.setValue(inherited);
        item.value.setValue(inherited ? documentState.effective[id].value : draft[id]);
      }
    };
    const checkedDocument = (value) => {
      if (!value || !Number.isSafeInteger(value.revision) || value.revision < 0 ||
          !value.values || !value.effective || parameters.some(([id]) => !Object.hasOwn(value.effective, id))) {
        throw new Error('Invalid RTX HDR settings response.');
      }
      return value;
    };
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
    container.replaceChildren(help, fields, saveButton.element, status);
    status.textContent = 'Loading RTX HDR settings…';
    void sdk.settings.read().then((value) => {
      if (sdk.signal.aborted) return;
      documentState = checkedDocument(value);
      draft = { ...documentState.values };
      baseline = { ...draft };
      for (const [id, label, type, minimum, maximum] of parameters) {
        const inherited = !Object.hasOwn(draft, id);
        const value = inherited ? documentState.effective[id].value : draft[id];
        const options = { label, value, disabled: !sdk.settings.canWrite, onChange(value) {
          controls.get(id).inherit.setValue(false);
          changed(id, value, false);
        } };
        const control = type === 'boolean' ? sdk.ui.toggle(options) : sdk.ui.slider({ ...options, minimum, maximum, step: 1 });
        const inherit = sdk.ui.toggle({ label: `Use inherited ${label.toLowerCase()}`, value: inherited,
          disabled: !sdk.settings.canWrite, onChange(inherited) { changed(id, control.value(), inherited); } });
        const row = document.createElement('div');
        row.className = 'rtx-hdr-setting';
        row.append(control.element, inherit.element);
        fields.append(row);
        controls.set(id, { value: control, inherit });
      }
      sdk.setDirty(false);
      status.textContent = sdk.settings.canWrite ? 'RTX HDR settings are up to date.' : 'RTX HDR settings are read-only.';
    }).catch((error) => {
      if (!sdk.signal.aborted) status.textContent = `Unable to load RTX HDR settings: ${error.message}`;
    });
    const dispose = () => {
      cancelTimer();
      for (const item of controls.values()) { item.value.dispose(); item.inherit.dispose(); }
      controls.clear();
      saveButton.dispose();
    };
    sdk.signal.addEventListener('abort', dispose, { once: true });
    return dispose;
  },
});

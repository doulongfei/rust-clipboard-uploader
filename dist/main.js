window.addEventListener('DOMContentLoaded', async () => {
  const uploadUrl = document.getElementById('upload_url');
  const method = document.getElementById('method');
  const fileField = document.getElementById('file_field');
  const response = document.getElementById('response');
  const copy = document.getElementById('copy_to_clipboard');
  const status = document.getElementById('status');

  // load config via Tauri
  if (window.__TAURI__) {
    const { invoke } = window.__TAURI__;
    const cfg = await invoke('get_config');
    uploadUrl.value = cfg.upload_url || '';
    method.value = cfg.method || 'POST';
    fileField.value = cfg.file_field || 'file';
    response.value = cfg.response || 'text';
    copy.checked = cfg.copy_to_clipboard || false;

    document.getElementById('save').addEventListener('click', async () => {
      const newCfg = {
        upload_url: uploadUrl.value,
        method: method.value,
        file_field: fileField.value,
        response: response.value,
        copy_to_clipboard: copy.checked
      };
      const res = await invoke('save_config', newCfg);
      status.textContent = '已保存';
    });
  } else {
    status.textContent = '未在 Tauri 环境下运行，页面为静态预览';
  }
});
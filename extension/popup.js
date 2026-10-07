const input = document.getElementById('port');
const status = document.getElementById('status');

// One question to the worker: is Trove reachable, and which library is it on.
// Asking the service directly would work too, but then the popup would keep its
// own idea of "connected" separate from the one the icon shows.
async function report() {
  chrome.runtime.sendMessage({ type: 'trove-status' }, (reply) => {
    if (chrome.runtime.lastError || !reply) {
      status.textContent = '无法连接（端口可能是别的程序）：Trove 未运行，或浏览器拦截了请求';
      return;
    }
    status.textContent = reply.connected
      ? `已连接 ✓ ${reply.library ? `· ${reply.library}` : ''}${reply.collections ? ` · ${reply.collections.length} 个合集` : ''}`
      : '未连接：请先启动 Trove 桌面应用';
  });
}

chrome.storage.local.get('port', ({ port }) => {
  input.value = port || 23916;
});

document.getElementById('save').addEventListener('click', async () => {
  const port = Number(input.value) || 23916;
  await chrome.storage.local.set({ port });
  status.textContent = '端口已保存';
  report();
});

document.getElementById('ping').addEventListener('click', () => {
  status.textContent = '测试中…';
  report();
});

report();

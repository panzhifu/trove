const input = document.getElementById('port');
const status = document.getElementById('status');

chrome.storage.local.get('port', ({ port }) => {
  input.value = port || 23916;
});

document.getElementById('save').addEventListener('click', async () => {
  const port = Number(input.value) || 23916;
  await chrome.storage.local.set({ port });
  status.textContent = '端口已保存';
});

document.getElementById('ping').addEventListener('click', async () => {
  const port = Number(input.value) || 23916;
  status.textContent = '测试中…';
  try {
    const response = await fetch(`http://127.0.0.1:${port}/ping`);
    const text = await response.text();
    status.textContent = response.ok && text.includes('trove') ? '已连接 ✓' : text;
  } catch {
    status.textContent = `无法连接（端口 ${port}）：Trove 未运行，或浏览器拦截了请求`;
  }
});

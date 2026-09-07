const input = document.getElementById('port');
const status = document.getElementById('status');

chrome.storage.local.get('port', ({ port }) => {
  input.value = port || 23916;
});

document.getElementById('save').addEventListener('click', async () => {
  const port = Number(input.value) || 23916;
  await chrome.storage.local.set({ port });
  status.textContent = 'port saved';
});

document.getElementById('ping').addEventListener('click', async () => {
  const port = Number(input.value) || 23916;
  status.textContent = '…';
  try {
    const response = await fetch(`http://127.0.0.1:${port}/ping`);
    const text = await response.text();
    status.textContent = response.ok && text.includes('trove') ? 'connected ✓' : text;
  } catch (error) {
    status.textContent = 'no connection (start Trove)';
  }
});

// The three switches, straight onto `chrome.storage.sync`.
//
// A write that fails is said out loud: a preference the user flicked and that
// did not persist would otherwise look like a switch that does nothing, and the
// next save would behave differently from what the screen claims.
const KEYS = ['notifications', 'focusAfterSave', 'dragMenu'];

const status = document.getElementById('saved');

async function load() {
  const values = await TrovePrefs.all();
  for (const key of KEYS) {
    document.getElementById(key).checked = values[key];
  }
}

for (const key of KEYS) {
  const box = document.getElementById(key);
  box.addEventListener('change', async () => {
    const ok = await TrovePrefs.set(key, box.checked);
    if (ok) {
      status.textContent = '已保存';
      status.style.color = '#4ade80';
    } else {
      box.checked = !box.checked;
      status.textContent = '没能写入（浏览器拒绝了同步存储）；本次会话仍按新设置运行';
      status.style.color = '#f87171';
    }
    setTimeout(() => {
      status.textContent = '';
    }, 2500);
  });
}

load();

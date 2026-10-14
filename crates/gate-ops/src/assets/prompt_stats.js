const tables = document.querySelectorAll('table[data-num]');
tables.forEach(tb => {
  tb.querySelectorAll('th').forEach((th, i) => th.addEventListener('click', () => {
    const numeric = JSON.parse(tb.dataset.num)[i];
    const body = tb.tBodies[0], rows = [...body.rows];
    const dir = th.dataset.dir === '1' ? -1 : 1;
    tb.querySelectorAll('th').forEach(x => delete x.dataset.dir);
    th.dataset.dir = String(dir);
    rows.sort((a, b) => {
      const x = a.cells[i].textContent, y = b.cells[i].textContent;
      return (numeric ? (parseFloat(x) || 0) - (parseFloat(y) || 0) : x.localeCompare(y, 'zh')) * dir;
    });
    rows.forEach(r => body.appendChild(r));
  }));
});
function filterTable(input, id){
  const q = input.value.trim().toLowerCase();
  // 旧版用 querySelectorAll('#'+id)：第一张表的 id 里有「:」，选择器会抛错、搜索框失效；改按 id 取元素
  const tb = document.getElementById(id);
  if (!tb) return;
  tb.querySelectorAll('tbody tr').forEach(tr => {
    tr.hidden = q && !tr.textContent.toLowerCase().includes(q);
  });
}

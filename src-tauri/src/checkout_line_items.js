(errors) => {
  const rows = [...document.querySelectorAll('[data-autom^="bag-item-"]')].filter((e) => /^bag-item-\d+$/.test(e.getAttribute('data-autom')));
  const items = rows.map((row) => {
    const title = row.querySelector('[data-autom="bag-item-name"], h2, h3');
    const select = row.querySelector('select[data-autom="item-quantity-dropdown"]');
    const text = (row.innerText || '').replace(/\s+/g, ' ');
    const counted = text.match(/数量\s*(\d+)/);
    const link = row.querySelector('a[data-autom="bag-item-name"]');
    const part = link && (link.getAttribute('href') || '').match(/\/shop\/product\/([^/?#]+\/[^/?#]+)/i);
    const remove = row.querySelector('button[data-autom="bag-item-remove-button"]');
    return {
      name: (title ? title.innerText : '').replace(/\s+/g, ' ').trim(),
      quantity: select ? select.value : counted ? counted[1] : '',
      partNumber: part ? decodeURIComponent(part[1]).toUpperCase() : '',
      removeId: remove ? remove.id : '',
    };
  });
  const total = document.querySelector('[data-autom="bagtotalvalue"]');
  const checkout = document.querySelector('button[data-autom="checkout"]');
  const placeOrder = document.querySelector('button[data-autom="continue-button-placeOrder"]');
  return {
    items,
    total: total ? total.innerText.replace(/\s+/g, ' ').trim() : '',
    canCheckout: !!checkout && !checkout.disabled,
    canPlaceOrder: !!placeOrder && !placeOrder.disabled,
    // 没有商品行不代表空袋：导航加载中也没有商品行，必须等 Apple 的空袋提示。
    bagReady: location.pathname === '/shop/bag' && (!!document.querySelector('[data-autom="bag-header"]') || !!document.querySelector('[data-autom="bag-empty-continueshopping-button"]')),
    bagEmpty: !!document.querySelector('[data-autom="bag-empty-continueshopping-button"]') && items.length === 0,
    // 本 fork：打开购物袋前给旧文档打了标记（globalThis 在页面里就是 window），只有新文档
    // 加载完才算数；明确的空袋文字用于几秒内停下，不干等元素超时。
    loaded: !globalThis.__apwStaleBag && document.readyState === 'complete' && /\/shop\/bag/.test(location.pathname),
    emptyNotice: /购物袋(目前)?(是|为)空/.test((document.body ? document.body.innerText : '').replace(/\s+/g, '')),
    errors,
  };
}

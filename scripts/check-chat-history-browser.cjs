const { chromium } = require('playwright');
const assert = require('node:assert/strict');
(async () => {
  const browser = await chromium.launch({ executablePath: process.env.HIVE_CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    const errors=[]; page.on('pageerror', e=>errors.push(e.message));
    await page.goto(process.env.HIVE_TEST_BASE);
    await page.locator('[name=password]').fill('history-test-password');
    await page.getByRole('button',{name:'Sign in',exact:true}).click();
    await page.locator('#new-chat').waitFor();
    await page.locator('#input').fill('browser fixture <img src=x onerror=alert(1)>');
    await page.locator('#send').click();
    await page.locator('#feed .step pre').filter({hasText:'saved-chat-ok'}).first().waitFor();
    const chatUrl=page.url(); assert(chatUrl.includes('#chat='));
    await page.reload();
    await page.locator('#feed .step pre').filter({hasText:'saved-chat-ok'}).first().waitFor();
    assert.equal(await page.locator('#feed img').count(),0);
    await page.locator('#chat-search').fill('browser fixture');
    await page.waitForFunction(()=>document.querySelectorAll('#chat-list .chat-item').length===1);
    await page.locator('#new-chat').click();
    await page.locator('#chat-list .chat-item').click();
    await page.locator('#feed .step pre').filter({hasText:'saved-chat-ok'}).first().waitFor();
    await page.screenshot({path:'/tmp/hive-chat-history-desktop.png',fullPage:true});
    await page.setViewportSize({width:390,height:844});
    await page.locator('#history-toggle').click();
    await page.locator('#chat-list .chat-item').click();
    await page.locator('#feed .step pre').filter({hasText:'saved-chat-ok'}).first().waitFor();
    assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/hive-chat-history-phone.png',fullPage:true});
    assert.deepEqual(errors,[]);
    console.log('PASS: browser send, reload, search, reopen, HTML escaping, and phone layout');
  } finally { await browser.close(); }
})().catch(e=>{console.error(e);process.exitCode=1;});

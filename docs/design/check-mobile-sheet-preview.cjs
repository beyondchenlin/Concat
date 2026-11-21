// Preview-only interaction checks. These do not validate the native Android application.
const { chromium } = require('playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

(async () => {
  const file = path.join(__dirname, 'bk-cat-mobile-sheet-20261121.html');
  const evidence = process.env.BKCAT_PREVIEW_EVIDENCE || path.resolve(__dirname, '../../.build/mobile-sheet-preview-20261121');
  fs.mkdirSync(evidence, { recursive: true });
  const executablePath = process.env.BKCAT_PREVIEW_BROWSER
    || (fs.existsSync('/usr/lib/chromium/chromium') ? '/usr/lib/chromium/chromium' : undefined);
  const browser = await chromium.launch({ headless: true, executablePath });
  const errors = [];
  const results = [];
  const observations = {};
  const page = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 1 });
  page.on('pageerror', error => errors.push(String(error)));
  page.on('request', request => {
    if (/^https?:/.test(request.url())) errors.push('Unexpected network request: ' + request.url());
  });
  const snapshot = () => page.evaluate(() => window.bkcatPreview.snapshot());
  const check = (name, fn) => fn().then(() => results.push({ name, result: 'passed' }));
  async function capture(name) {
    // Pointer-driven checks may leave a browser text selection. Clear only
    // that transient test state; the preview keeps its normal text selection.
    await page.evaluate(() => window.getSelection()?.removeAllRanges());
    await page.screenshot({ path: path.join(evidence, name) });
  }
  async function dragGrip(delta) {
    const grip = await page.locator('#grip').boundingBox();
    const x = grip.x + grip.width / 2;
    const y = grip.y + grip.height / 2;
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x, y + delta, { steps: 12 });
    await page.mouse.up();
    await page.waitForTimeout(220);
  }
  try {
    await page.goto(pathToFileURL(file).href);
    await check('new page opens the existing editor with the assistant closed', async () => {
      assert.equal((await snapshot()).sheet, 'closed');
      assert.equal(await page.locator('#editor').isVisible(), true);
      assert.equal(await page.locator('#tools .tool').first().innerText(), '媒体');
      await page.evaluate(() => { window.initialEditor = document.querySelector('#editor'); });
    });
    await capture('01-editor-390.png');
    await check('opening and resizing preserves clip selection and playhead without remounting the editor', async () => {
      await page.locator('#video-clip').click();
      await page.locator('#step-next').click();
      const before = await snapshot();
      await page.locator('#assistant-open').click();
      assert.equal((await snapshot()).sheet, 'half');
      await page.waitForTimeout(220);
      await dragGrip(-110);
      assert.equal((await snapshot()).sheet, 'full');
      await dragGrip(110);
      assert.equal((await snapshot()).sheet, 'half');
      await dragGrip(115);
      assert.equal((await snapshot()).sheet, 'closed');
      const after = await snapshot();
      assert.equal(after.time, before.time);
      assert.equal(after.selected, before.selected);
      assert.equal(await page.evaluate(() => document.querySelector('#editor') === window.initialEditor), true);
    });
    await check('dragging chat content does not resize the sheet', async () => {
      await page.locator('#assistant-open').click();
      await page.waitForTimeout(220);
      const box = await page.locator('#messages').boundingBox();
      await page.mouse.move(box.x + 10, box.y + Math.min(70, box.height / 2));
      await page.mouse.down();
      await page.mouse.move(box.x + 10, box.y + 6, { steps: 10 });
      await page.mouse.up();
      assert.equal((await snapshot()).sheet, 'half');
    });
    await capture('02-half-sheet-390.png');
    await check('request waits for explicit approval; hiding the sheet never approves it', async () => {
      await page.locator('#suggestion').click();
      await page.locator('#approve').waitFor();
      assert.equal((await snapshot()).caption, false);
      assert.equal((await snapshot()).pending, true);
      await page.locator('#sheet-close').click();
      assert.equal((await snapshot()).pending, true);
      assert.equal((await snapshot()).caption, false);
      await page.locator('#assistant-open').click();
      await page.locator('#approve').click();
      assert.equal((await snapshot()).caption, true);
      assert.equal((await snapshot()).pending, false);
      assert.equal((await snapshot()).undoDepth, 1);
    });
    await page.locator('#sheet-expand').click();
    await page.waitForTimeout(220);
    await check('approved full-screen conversation keeps the composer visible and usable at the bottom', async () => {
      const current = await snapshot();
      assert.equal(current.sheet, 'full');
      assert.equal(current.caption, true);
      assert.equal(current.pending, false);
      const layout = await page.evaluate(() => {
        const rect = id => {
          const element = document.getElementById(id);
          const r = element.getBoundingClientRect();
          const style = getComputedStyle(element);
          const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
          return { id, left: r.left, right: r.right, top: r.top, bottom: r.bottom,
            width: r.width, height: r.height,
            visible: !element.hidden && style.display !== 'none' && style.visibility !== 'hidden',
            unobstructed: element === hit || element.contains(hit) };
        };
        return { width: innerWidth, height: innerHeight,
          sheet: rect('assistant-sheet'), messages: rect('messages'),
          controls: ['composer', 'prompt', 'send', 'permissions'].map(rect) };
      });
      for (const control of layout.controls) {
        assert.ok(control.visible && control.width > 0 && control.height > 0,
          control.id + ' is hidden after approval in full screen');
        assert.ok(control.left >= -1 && control.right <= layout.width + 1,
          control.id + ' overflows the full-screen viewport width');
        assert.ok(control.top >= layout.sheet.top - 1
          && control.bottom <= Math.min(layout.height, layout.sheet.bottom) + 1,
          control.id + ' is clipped below the approved full-screen conversation');
        assert.ok(control.unobstructed, control.id + ' is covered by another element');
      }
      const composer = layout.controls.find(control => control.id === 'composer');
      assert.ok(layout.messages.bottom <= composer.top + 1,
        'the full-screen transcript overlaps the composer');
      assert.ok(Math.abs(composer.bottom - layout.sheet.bottom) <= 1,
        'the composer is not anchored to the full-screen sheet bottom');
      await page.locator('#prompt').fill('Composer visibility check');
      assert.equal(await page.locator('#send').isEnabled(), true);
      await page.locator('#send').click({ trial: true });
      await page.locator('#prompt').fill('');
      await page.locator('#prompt').evaluate(element => { element.style.height = ''; });
      await page.locator('#grip').focus();
      observations.approvedFullScreen = layout;
    });
    await capture('03-full-sheet-390.png');
    await check('return steps through full, half and closed; undo and redo share the sample project history', async () => {
      await page.locator('#sheet-back').click();
      assert.equal((await snapshot()).sheet, 'half');
      await page.locator('#sheet-back').click();
      assert.equal((await snapshot()).sheet, 'closed');
      await page.locator('#undo').click();
      assert.equal((await snapshot()).caption, false);
      await page.locator('#redo').click();
      assert.equal((await snapshot()).caption, true);
    });
    await capture('04-caption-added-390.png');
    await check('stop prevents delayed sample approval and project mutation', async () => {
      await page.evaluate(() => window.bkcatPreview.reset());
      await page.locator('#assistant-open').click();
      await page.locator('#prompt').fill('测试停止');
      await page.locator('#send').click();
      await page.locator('#stop').click();
      await page.waitForTimeout(850);
      const current = await snapshot();
      assert.equal(current.busy, false);
      assert.equal(current.pending, false);
      assert.equal(current.caption, false);
    });
    await check('new project defaults to the original editor', async () => {
      await page.locator('#sheet-close').click();
      await page.locator('#project-back').click();
      assert.equal((await snapshot()).projectOpen, false);
      await page.locator('#new-project').click();
      assert.equal((await snapshot()).projectOpen, true);
      assert.equal((await snapshot()).sheet, 'closed');
      assert.equal(await page.locator('#project-name').innerText(), '新建工程');
    });
    await check('full-screen pending approval keeps the composer usable when available viewport height is reduced', async () => {
      // Browser viewport reduction exercises layout only. It is not an Android IME test.
      await page.setViewportSize({ width: 390, height: 844 });
      await page.evaluate(() => window.bkcatPreview.reset());
      await page.locator('#assistant-open').click();
      await page.locator('#sheet-expand').click();
      await page.locator('#suggestion').click();
      await page.locator('#approve').waitFor();
      await page.locator('#prompt').focus();
      await page.setViewportSize({ width: 390, height: 420 });
      await page.waitForTimeout(240);
      await page.locator('#approve').scrollIntoViewIfNeeded();
      const layout = await page.evaluate(() => {
        const controls = ['prompt', 'send', 'permissions', 'approve', 'reject'].map(id => {
          const element = document.getElementById(id);
          const rect = element.getBoundingClientRect();
          const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
          return { id, left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom,
            width: rect.width, height: rect.height, unobstructed: element === hit || element.contains(hit) };
        });
        return { width: innerWidth, height: innerHeight, controls };
      });
      for (const control of layout.controls) {
        assert.ok(control.width > 0 && control.height > 0, control.id + ' disappeared in the reduced viewport');
        assert.ok(control.left >= -1 && control.right <= layout.width + 1,
          control.id + ' exceeds the reduced viewport width');
        assert.ok(control.top >= -1 && control.bottom <= layout.height + 1,
          control.id + ' exceeds the reduced viewport height');
        assert.ok(control.unobstructed, control.id + ' is obstructed in the reduced viewport');
      }
      assert.equal((await snapshot()).sheet, 'full');
      assert.equal((await snapshot()).pending, true);
      assert.equal((await snapshot()).caption, false);
      assert.equal(await page.locator('#send').isDisabled(), true);
      await page.locator('#prompt').fill('待审批时保留下一条草稿');
      assert.equal(await page.locator('#prompt').inputValue(), '待审批时保留下一条草稿');
      observations.pendingApprovalReducedViewport = layout;
      await capture('06-full-pending-reduced-390x420.png');
      await page.locator('#reject').click();
      assert.equal((await snapshot()).pending, false);
      assert.equal((await snapshot()).caption, false);
    });
    await check('320, 390, 430 and landscape layouts keep content and primary controls in bounds', async () => {
      for (const [width, height] of [[320,740],[390,844],[430,932],[720,400]]) {
        await page.setViewportSize({ width, height });
        await page.evaluate(() => window.bkcatPreview.reset());
        await page.locator('#assistant-open').click();
        await page.waitForTimeout(240);
        const layout = await page.evaluate(() => {
          const ids=['assistant-sheet','sheet-grab','prompt','send','permissions'];
          return { width:innerWidth,height:innerHeight,documentWidth:document.documentElement.scrollWidth,rects:ids.map(id=>{const r=document.getElementById(id).getBoundingClientRect();return{id,left:r.left,right:r.right,top:r.top,bottom:r.bottom};}) };
        });
        assert.ok(layout.documentWidth<=width+1, 'horizontal page overflow at '+width);
        for (const rect of layout.rects) {
          assert.ok(rect.left>=-1&&rect.right<=width+1, rect.id+' overflows width '+width);
          assert.ok(rect.top>=-1&&rect.bottom<=height+1, rect.id+' overflows height '+height);
        }
        await capture(`05-layout-${width}x${height}.png`);
      }
    });
    assert.deepEqual(errors,[]);
    results.push({name:'no script errors or network requests',result:'passed'});
    const report={scope:'Standalone HTML preview only, not native Android acceptance',source:path.basename(file),passed:results.length,results,errors,observations};
    fs.writeFileSync(path.join(evidence,'preview-check-results.json'),JSON.stringify(report,null,2)+'\n');
    console.log(JSON.stringify(report,null,2));
  } finally {
    await browser.close();
  }
})().catch(error=>{console.error(error);process.exitCode=1;});

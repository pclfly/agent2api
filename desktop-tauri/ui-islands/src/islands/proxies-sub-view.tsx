/**
 * 「网络代理 → 订阅」区块：订阅增删改、刷新、内核状态与下载。
 *
 * ── 与旧版订阅界面的差别（按实际数据形状重写）─────────────────
 * 旧版（v3.1.0 时代）的界面按「订阅 → 节点」两级展开，节点端口从内核状态里
 * 现算。本版的数据形状已经不同：
 *
 *   1. 订阅列表的 `nodeCount` 由后端直接给出（按 `kernel.providerSync` 汇总），
 *      不需要前端再算一次；
 *   2. 订阅节点在代理池里的投影是 `source=subscription`、协议恒 `http`、
 *      主机恒 `127.0.0.1`（端口由内核分配）—— 节点明细在**代理池页**看，
 *      这里只显示「同步到几个节点」；
 *   3. 内核是独立进程，`installed` / `running` / `listenerCount` 是三个独立的
 *      事实，把它们合成一个「状态」会丢掉「装了但没跑」这种常见情形。
 *
 * 所以这里**不做二级展开**：订阅行给出节点数与同步时刻，节点本身去池里看。
 */

import * as React from 'react'
import {
  Badge, Button, Dialog, DialogBody, DialogContent,
  DialogFooter, DialogHeader, DialogTitle, Input, Label, Switch,
} from '@ui'
import { t } from '../i18n'
import {
  addLocalSubscription, addSubscription, cancelKernelDownload, decodeSubscriptionQr, downloadKernel, downloadPercent,
  getSnapshot, kernelPhase, loadSubscriptions, refreshSubscription, removeKernel,
  removeSubscription, restartKernel, saveSubscription, setSubscriptionEnabled,
  stopKernel, subscribe, type KernelStatus, type SubscriptionItem,
} from './proxies-sub-state'
// 精确时刻的格式化在 proxies-state（`wbApp.formatTime` 的薄封装，与池表同一套）
import { formatTime } from './proxies-state'

/** 链接截断（表格里只显示主机段；完整链接挂在 title 上） */
function shortUrl(url: string): string {
  try {
    const parsed = new URL(url)
    return parsed.host + (parsed.pathname !== '/' ? '/…' : '')
  } catch {
    return url.slice(0, 60)
  }
}

/**
 * 同步时间的相对说法（「3 分钟前」）。
 *
 * 这一列回答的是「上一次成功拉取离现在多久」—— 绝对值（`14:32:07`）要用户
 * 自己在脑子里做减法，而「内核是不是一直在正常刷新」这个判断靠的正是间隔。
 * 精确时刻挂在 title 上（`formatTime`），需要核对时悬停即可。
 *
 * 时钟回拨 / 未来时间统一显示「刚刚」：负数差值说明本机时间被动过，
 * 显示「-3 分钟前」只会让人以为是 bug。
 */
function relativeTime(at: number): string {
  const delta = Date.now() - at
  if (!Number.isFinite(delta) || delta < 60_000) return t('刚刚')
  const minutes = Math.floor(delta / 60_000)
  if (minutes < 60) return t('{n} 分钟前', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return t('{n} 小时前', { n: hours })
  const days = Math.floor(hours / 24)
  return t('{n} 天前', { n: days })
}

/* ─── 内核状态条 ─────────────────────────────── */

/**
 * 内核状态条：徽标 + 版本 / 监听器数 + 内核设置入口。
 *
 * 两处使用：页面顶部（内核与节点端口是**全局**的事 —— 端口用尽的报错也属于
 * 它，跟着订阅表一起收进弹窗就看不见了）与订阅区块内的嵌用。
 */
export function KernelBar({ kernel, onOpen }: { kernel: KernelStatus | null; onOpen: () => void }) {
  const phase = kernelPhase(kernel)
  const percent = downloadPercent(kernel)
  const downloading = kernel?.download.state === 'downloading'

  const tone = phase === 'running' ? 'ok' : phase === 'stopped' ? 'warn' : 'muted'
  const label = phase === 'running'
    ? t('内核运行中')
    : phase === 'stopped'
      ? t('内核已安装，未运行')
      : t('内核未安装')

  return (
    <div className='sub-kernel-bar'>
      <span className={`sub-kernel-dot sub-kernel-dot-${tone}`} aria-hidden='true' />
      <span className='sub-kernel-label'>{label}</span>
      {phase !== 'missing' ? (
        <span className='sub-kernel-meta'>
          {t('版本 {version}', { version: kernel?.version || '—' })}
          {kernel?.running ? t(' · {count} 个节点在监听', { count: kernel.listenerCount }) : null}
        </span>
      ) : null}
      {downloading ? (
        <span className='sub-kernel-meta'>
          {percent ? t('下载中 {percent}%', { percent }) : t('下载中…')}
        </span>
      ) : null}
      {kernel?.lastError ? (
        <span className='sub-kernel-error' title={kernel.lastError}>{kernel.lastError}</span>
      ) : null}
      <span className='sub-kernel-actions'>
        <Button variant='outline' size='sm' onClick={onOpen}>
          {t('内核设置')}
        </Button>
      </span>
    </div>
  )
}

/* ─── 新增 / 编辑弹窗 ─────────────────────────── */

function SubscriptionModal({
  item, disabled, onClose,
}: {
  item: SubscriptionItem | null
  disabled: boolean
  onClose: () => void
}) {
  const [name, setName] = React.useState(item?.name ?? '')
  const [url, setUrl] = React.useState(item?.url ?? '')
  // 来源：编辑已有本地订阅时默认 local，其余默认 url
  const [kind, setKind] = React.useState(item?.kind === 'local' ? 'local' : 'url')
  const [localFile, setLocalFile] = React.useState<{ name: string; dataUrl: string } | null>(null)
  // 节点名过滤词（空格 / 逗号分隔，空 = 不过滤）
  const [exclude, setExclude] = React.useState(item?.exclude ?? '')
  const [error, setError] = React.useState('')
  const [saving, setSaving] = React.useState(false)
  const [decoding, setDecoding] = React.useState(false)
  const busy = disabled || saving || decoding

  /** 把用户选的文件读成 data URL（二维码图片与 YAML 共用这一条读法）。 */
  function readAsDataUrl(file: File, then: (value: string) => void): void {
    const reader = new FileReader()
    reader.onload = () => then(typeof reader.result === 'string' ? reader.result : '')
    reader.onerror = () => setError(t('读取文件失败'))
    reader.readAsDataURL(file)
  }

  async function pickQr(file: File | null): Promise<void> {
    if (!file) return
    setError('')
    setDecoding(true)
    readAsDataUrl(file, value => {
      void (async () => {
        const result = await decodeSubscriptionQr(value)
        setDecoding(false)
        if (!result.ok || !result.url) { setError(result.error || t('识别失败')); return }
        setUrl(result.url)
        setKind('url')
      })()
    })
  }

  async function commit() {
    const trimmedName = name.trim()
    if (!trimmedName) { setError(t('请填写订阅名称')); return }
    const trimmedExclude = exclude.trim()
    setSaving(true)
    let result: { ok: boolean; error?: string }
    if (kind === 'local') {
      // 编辑时没重新选文件 = 内容不变，只有改名 / 改过滤词的情形走 save
      if (!localFile && item?.kind === 'local') {
        result = await saveSubscription({ id: item.id, name: trimmedName, url: '', exclude: trimmedExclude })
      } else if (!localFile) {
        setSaving(false)
        setError(t('请选择要导入的 YAML 文件'))
        return
      } else {
        result = item
          ? await saveSubscription({
              id: item.id, name: trimmedName, url: '', exclude: trimmedExclude,
              contentBase64: localFile.dataUrl, fileName: localFile.name,
            })
          : await addLocalSubscription({
              name: trimmedName, contentBase64: localFile.dataUrl, fileName: localFile.name,
              exclude: trimmedExclude,
            })
      }
    } else {
      const trimmedUrl = url.trim()
      if (!trimmedUrl) { setSaving(false); setError(t('请填写订阅链接')); return }
      result = item
        ? await saveSubscription({ id: item.id, name: trimmedName, url: trimmedUrl, exclude: trimmedExclude })
        : await addSubscription({ name: trimmedName, url: trimmedUrl, exclude: trimmedExclude })
    }
    setSaving(false)
    if (!result.ok) { setError(result.error || t('保存失败')); return }
    onClose()
  }

  return (
    <Dialog open onOpenChange={open => { if (!open && !busy) onClose() }}>
      {/* 这一层是「订阅管理」弹窗之上的**二级弹窗**：Base UI 的嵌套约定是
          子层不画自己的遮罩，而「点空白处关闭」认的正是遮罩本身 —— 不画遮罩
          就点哪儿都不关（只剩 Esc）。`overlayForceRender` 显式补上它，
          子层因此既能被点空白关闭、又能把父层压暗以示层级。 */}
      <DialogContent className='w-[min(560px,calc(100vw-48px))]' overlayForceRender>
        <DialogHeader>
          <DialogTitle>{item ? t('编辑订阅') : t('添加订阅')}</DialogTitle>
        </DialogHeader>
        <DialogBody>
          <div className='grid gap-3'>
            <div className='grid gap-1.5'>
              <Label>{t('名称')}</Label>
              <Input value={name} disabled={busy} placeholder={t('例如：主机场')}
                onChange={event => setName(event.currentTarget.value)} />
            </div>

            {/* 来源切换：链接订阅 / 本地 YAML 文件。
                两类的刷新语义不同 —— 链接由内核定时拉，本地文件只在重导入时变。 */}
            <div className='grid gap-1.5'>
              <Label>{t('来源')}</Label>
              <div className='flex gap-2'>
                <Button variant={kind === 'url' ? 'default' : 'outline'} size='sm' disabled={busy}
                  onClick={() => setKind('url')}>{t('订阅链接')}</Button>
                <Button variant={kind === 'local' ? 'default' : 'outline'} size='sm' disabled={busy}
                  onClick={() => setKind('local')}>{t('本地 YAML 文件')}</Button>
              </div>
            </div>

            {kind === 'url' ? (
              <div className='grid gap-1.5'>
                <Label>{t('订阅链接')}</Label>
                <Input value={url} disabled={busy} placeholder='https://…'
                  onChange={event => setUrl(event.currentTarget.value)} />
                <div className='flex flex-wrap items-center gap-2'>
                  <label className='inline-flex cursor-pointer items-center gap-1 text-sm text-subtle'>
                    <input type='file' accept='image/png,image/jpeg,image/webp' className='sr-only'
                      disabled={busy}
                      onChange={event => {
                        const file = event.currentTarget.files?.[0] ?? null
                        event.currentTarget.value = ''
                        void pickQr(file)
                      }} />
                    <span className='underline'>{decoding ? t('识别中…') : t('从二维码图片识别')}</span>
                  </label>
                  <span className='text-sm text-subtle'>
                    {t('可以截图机场的订阅二维码再选这张图；识别成功后链接会自动填到上面。')}
                  </span>
                </div>
                <p className='text-sm text-subtle'>
                  {t('粘贴机场的订阅链接。内核会按它拉取节点，并把节点写进代理池供账号引用；内容由内核定时刷新（每 24 小时），节点数与同步状态在保存后由轮询刷新，稍等几秒。')}
                </p>
              </div>
            ) : (
              <div className='grid gap-1.5'>
                <Label>{t('YAML 文件')}</Label>
                <input type='file' accept='.yaml,.yml,text/yaml,application/x-yaml' disabled={busy}
                  onChange={event => {
                    const file = event.currentTarget.files?.[0] ?? null
                    event.currentTarget.value = ''
                    if (!file) return
                    readAsDataUrl(file, value => {
                      if (!value) return
                      setLocalFile({ name: file.name, dataUrl: value })
                      if (!name.trim()) setName(file.name.replace(/\.[^.]+$/, ''))
                    })
                  }}
                  className='text-sm' />
                <p className='text-sm text-subtle'>
                  {localFile
                    ? t('已选择：{name}', { name: localFile.name })
                    : item?.kind === 'local'
                      ? t('当前是本地文件订阅；不重新选文件表示内容不变，只改名称。')
                      : t('选择一份 Clash / mihomo 的 YAML 配置（上限 2MB）。')}
                </p>
                {/* 本地文件的刷新语义与在线订阅**正好相反**：内容在导入那一刻
                    就固定了，没有定时刷新 —— 不说清楚用户会等它自己变 */}
                <p className='text-sm text-subtle'>
                  {t('本地导入的 YAML 不做定时刷新：内容是导入那一刻的快照，更新请重新选择文件。')}
                </p>
              </div>
            )}

            {/* 节点名过滤词：机场的订阅里常混着「剩余流量」「到期时间」「招代理」
                这类假节点，它们在池里既不能选也不能测，只会把列表撑乱。这里按
                名字挡掉，挡掉的节点连同内核配置一起不生成（不是只在界面上隐藏）。 */}
            <div className='grid gap-1.5'>
              <Label>{t('节点名过滤词')}</Label>
              <Input value={exclude} disabled={busy}
                placeholder={t('例如：剩余流量 到期 招代理')}
                onChange={event => setExclude(event.currentTarget.value)} />
              <p className='text-sm text-subtle'>
                {t('空格或逗号分隔，可填多个；节点名含其中任一词就不纳入（不进内核、不进代理池）。留空表示全部保留。保存后点「立即更新」或重启内核生效。')}
              </p>
            </div>

            {error ? <p className='text-sm text-danger'>{error}</p> : null}
          </div>
        </DialogBody>
        <DialogFooter>
          <Button variant='outline' disabled={busy} onClick={onClose}>{t('取消')}</Button>
          <Button disabled={busy} onClick={() => void commit()}>{saving ? t('保存中…') : t('保存')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/* ─── 内核弹窗 ───────────────────────────────── */

/**
 * 内核弹窗：下载 / 启动重启 / 停止 / 删除内核（完整生命周期）。
 *
 * 两处使用：订阅区块内（`SubscriptionSection` 的「内核设置」）与网络代理页
 * 顶部的内核状态条（那里也摆一颗「内核设置」—— 状态条本身是页面级的）。
 */
export function KernelDialog({ kernel, busy, onClose }: { kernel: KernelStatus | null; busy: string; onClose: () => void }) {
  const phase = kernelPhase(kernel)
  const percent = downloadPercent(kernel)
  const downloading = kernel?.download.state === 'downloading'
  const locked = !!busy

  return (
    <Dialog open onOpenChange={open => { if (!open) onClose() }}>
      {/* 与订阅表单同理：它是「订阅管理」之上的二级弹窗，必须自带遮罩，
          否则点空白处关不掉（见 SubscriptionModal 的说明） */}
      <DialogContent className='w-[min(640px,calc(100vw-48px))]' overlayForceRender>
        <DialogHeader>
          <DialogTitle>{t('代理内核')}</DialogTitle>
        </DialogHeader>
        <DialogBody>
          <div className='grid gap-3 text-sm'>
            <p className='text-subtle'>
              {t('内核（mihomo）把订阅里的节点转成本机 http 监听器，账号按端口引用它们。它作为独立进程运行，网关负责下载、启动与停止。')}
            </p>
            <div className='grid gap-1'>
              <span>{t('状态：{status}', {
                status: phase === 'running' ? t('运行中')
                  : phase === 'stopped' ? t('已停止')
                    : t('未安装'),
              })}</span>
              <span>{t('版本：{version}', { version: kernel?.version || '—' })}</span>
              <span>{t('存放目录：{dir}', { dir: kernel?.kernelDir || '—' })}</span>
              {kernel?.running ? <span>{t('监听节点数：{count}', { count: kernel.listenerCount })}</span> : null}
            </div>
            {downloading ? (
              <div className='grid gap-1'>
                <span>{percent ? t('下载中 {percent}%', { percent }) : t('下载中…')}</span>
                {!percent ? <span className='text-subtle'>{t('正在准备下载…')}</span> : null}
              </div>
            ) : null}
            {kernel?.lastError ? <p className='text-danger'>{kernel.lastError}</p> : null}
            {kernel?.download.error ? <p className='text-danger'>{kernel.download.error}</p> : null}
          </div>
        </DialogBody>
        <DialogFooter>
          {phase === 'missing' ? (
            <Button disabled={locked} onClick={() => void downloadKernel()}>
              {t('下载内核')}
            </Button>
          ) : null}
          {downloading ? (
            <Button variant='outline' disabled={locked} onClick={() => void cancelKernelDownload()}>
              {t('取消下载')}
            </Button>
          ) : null}
          {phase !== 'missing' && !downloading ? (
            <>
              <Button variant='outline' disabled={locked} onClick={() => void restartKernel()}>
                {kernel?.running ? t('重启内核') : t('启动内核')}
              </Button>
              <Button variant='outline' disabled={locked || !kernel?.running} onClick={() => void stopKernel()}>
                {t('停止内核')}
              </Button>
              <Button
                variant='outline' disabled={locked}
                onClick={() => {
                  if (!window.confirm(t('删除内核？订阅节点会失去端口，账号出口会中断。'))) return
                  void removeKernel()
                }}
              >
                {t('删除内核')}
              </Button>
            </>
          ) : null}
          <Button disabled={locked} onClick={onClose}>{t('关闭')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/* ─── 区块本体 ───────────────────────────────── */

/**
 * 「订阅管理」弹窗。
 *
 * ── 为什么是弹窗而不是页面上的第二个面板（照 Agent2API 的布局）─────
 * 订阅表是「配完就不常动」的低频内容，而下面的代理池是天天要看的。两张表
 * 叠在同一页会把页面拉得很长，真正高频的那张被推到首屏之外。收进弹窗之后
 * 页面只剩代理池一张表，「订阅管理」按钮带条数 —— 不看也知道有几条。
 *
 * ── 刷新语义（两种来源不同，界面上要分开说）──────────────────────
 *   · **在线订阅**（`kind: url`）：内容由内核定时刷新，**每 24 小时**一次；
 *     保存后节点数与同步状态由轮询刷新（几秒内到位）。
 *   · **本地 YAML**（`kind: local`）：内容在导入那一刻就固定了，**没有定时
 *     刷新** —— 更新 = 编辑并重新选择文件。这一点必须在文案里写明，否则用户
 *     会等着它自己变。
 */
export function SubscriptionManagerDialog({ onClose }: { onClose: () => void }) {
  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot)
  const [modal, setModal] = React.useState<{ item: SubscriptionItem | null } | null>(null)
  const [kernelOpen, setKernelOpen] = React.useState(false)

  React.useEffect(() => {
    // 打开弹窗就自拉一次：订阅是唯一入口，不能停在上一份快照上
    void loadSubscriptions()
  }, [])

  return (
    <Dialog open onOpenChange={open => { if (!open && !snapshot.busy) onClose() }}>
      <DialogContent className='w-[min(880px,calc(100vw-48px))]'>
        <DialogHeader>
          <DialogTitle>{t('订阅管理')}</DialogTitle>
          <span className='text-[11.5px] text-subtle'>
            {snapshot.status === 'loading'
              ? t('加载中…')
              : t('{n} 条订阅', { n: snapshot.items.length })}
            {'　·　'}
            {t('在线订阅每 24 小时由内核自动刷新；本地 YAML 不刷新')}
          </span>
        </DialogHeader>
        <DialogBody className='flex flex-col gap-3'>
          <KernelBar kernel={snapshot.kernel} onOpen={() => setKernelOpen(true)} />

          {snapshot.status === 'error' ? (
            <p className='text-sm text-danger'>{snapshot.error}</p>
          ) : null}

          <div className='models-table-wrap'>
            <table className='models-table'>
              <colgroup>
                <col className='w-[18%]' />
                <col className='w-[24%]' />
                <col className='w-[8%]' />
                <col className='w-[15%]' />
                <col className='w-[8%]' />
                <col className='w-[27%]' />
              </colgroup>
              <thead>
                <tr>
                  <th>{t('名称')}</th>
                  <th>{t('订阅来源')}</th>
                  <th className='ta-center'>{t('节点')}</th>
                  <th>{t('上次同步')}</th>
                  <th className='ta-center'>{t('启用')}</th>
                  <th className='ta-right'>{t('操作')}</th>
                </tr>
              </thead>
              <tbody>
                {snapshot.items.length ? snapshot.items.map(item => {
                  const local = item.kind === 'local' || item.kind === 'file'
                  const isBusy = snapshot.busy === `refresh:${item.id}`
                    || snapshot.busy === `toggle:${item.id}`
                    || snapshot.busy === `remove:${item.id}`
                  return (
                    <tr key={item.id} className={item.enabled ? '' : 'off'}>
                      <td><b>{item.name || t('未命名')}</b></td>
                      <td>
                        <div className='flex min-w-0 items-center gap-1'>
                          {local ? (
                            <>
                              <span className='min-w-0 truncate font-mono text-[12px] text-text-2'
                                title={item.fileName}>{item.fileName || t('本地 YAML 文件')}</span>
                              <Badge variant='secondary' shape='tag'>{t('本地文件')}</Badge>
                            </>
                          ) : (
                            <>
                              <span className='min-w-0 truncate font-mono text-[12px] text-text-2'
                                title={item.url}>{shortUrl(item.url)}</span>
                              <Badge variant='secondary' shape='tag'>{t('链接')}</Badge>
                            </>
                          )}
                        </div>
                      </td>
                      <td className='ta-center'>
                        {item.nodeCount > 0
                          ? <Badge variant='secondary'>{t('{n} 个', { n: item.nodeCount })}</Badge>
                          : <span className='text-subtle'>{t('—')}</span>}
                      </td>
                      {/* 上次同步：取内核快照里的 `providerSync[<subId>].at`，
                          而不是订阅条目上的 `updatedAt` —— 后者是「条目被改过」
                          的时刻（改名 / 启停都会变），与「节点同步过」是两件事。
                          本地 YAML 没有远端可拉，同步只发生在导入那一刻，
                          那一列写「导入时」比一个时间戳更贴切。 */}
                      <td>
                        {(() => {
                          const sync = snapshot.kernel?.providerSync?.[item.id]
                          if (sync?.at) {
                            return <span className='text-[12px] text-text-2' title={formatTime(sync.at)}>
                              {relativeTime(sync.at)}
                            </span>
                          }
                          return <span className='text-subtle'>
                            {local ? t('导入时') : t('未同步')}
                          </span>
                        })()}
                      </td>
                      <td className='ta-center'>
                        <Switch checked={item.enabled} disabled={!!snapshot.busy}
                          aria-label={t('启用「{name}」', { name: item.name })}
                          onCheckedChange={checked => void setSubscriptionEnabled(item, checked === true)} />
                      </td>
                      <td>
                        <div className='row-actions'>
                          <Button variant='ghost' size='sm' disabled={isBusy || !!snapshot.busy || !item.enabled}
                            title={local
                              ? t('本地文件订阅的内容在导入时固定，更新请「编辑」并重新选择文件')
                              : t('让内核立刻重新拉取这条订阅（平时每 24 小时自动刷新一次）')}
                            onClick={() => void refreshSubscription(item.id)}>
                            {isBusy ? t('更新中…') : t('立即更新')}
                          </Button>
                          <Button variant='ghost' size='sm' disabled={isBusy || !!snapshot.busy}
                            onClick={() => setModal({ item })}>{t('编辑')}</Button>
                          <Button variant='destructive' size='sm' disabled={isBusy || !!snapshot.busy}
                            onClick={() => {
                              if (!window.confirm(t('删除订阅「{name}」？已同步的节点会从代理池移除。', { name: item.name }))) return
                              void removeSubscription(item.id)
                            }}>{t('删除')}</Button>
                        </div>
                      </td>
                    </tr>
                  )
                }) : (
                  <tr>
                    <td colSpan={6} className='empty'>
                      {snapshot.status === 'loading' ? t('加载中…') : (
                        <div className='flex flex-col items-center gap-2 py-3'>
                          <span>{t('还没有订阅：粘贴订阅链接，或导入本地 YAML 文件。')}</span>
                          <Button size='sm' variant='outline' disabled={!!snapshot.busy}
                            onClick={() => setModal({ item: null })}>{t('＋ 新增订阅')}</Button>
                        </div>
                      )}
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>

          <p className='text-xs leading-[1.65] text-subtle'>
            {t('在线订阅的内容由内核定时刷新（每 24 小时），节点数与同步状态在保存后由轮询刷新，稍等几秒；本地导入的 YAML 不做定时刷新，更新请「编辑」并重新选择文件。')}
          </p>
        </DialogBody>
        <DialogFooter>
          <Button size='sm' variant='outline' disabled={!!snapshot.busy}
            onClick={() => void loadSubscriptions()}>{t('刷新列表')}</Button>
          <Button size='sm' variant='outline' disabled={!!snapshot.busy}
            onClick={() => setKernelOpen(true)}>{t('内核设置')}</Button>
          <div className='mr-auto' />
          <Button size='sm' disabled={!!snapshot.busy} onClick={() => setModal({ item: null })}>
            {t('＋ 新增订阅')}
          </Button>
          <Button size='sm' variant='outline' onClick={onClose}>{t('完成')}</Button>
        </DialogFooter>
      </DialogContent>

      {modal ? (
        <SubscriptionModal
          item={modal.item}
          disabled={!!snapshot.busy}
          onClose={() => setModal(null)}
        />
      ) : null}
      {kernelOpen ? (
        <KernelDialog
          kernel={snapshot.kernel}
          busy={snapshot.busy}
          onClose={() => setKernelOpen(false)}
        />
      ) : null}
    </Dialog>
  )
}

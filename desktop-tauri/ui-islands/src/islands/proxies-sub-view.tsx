/**
 * Agent2API · 「订阅」区块（机场/节点订阅 → mihomo 内核本地监听器）。
 *
 * 与 Clash Verge 的订阅导入同一形态：粘贴订阅链接（或从图片解二维码）导入，
 * 内核拉取订阅、把每个节点起成本地 mixed 监听器，节点镜像进下面的代理池
 * （`source: "subscription"` 的只读条目，账号按 id 引用）。存储与内核管理在
 * Rust 侧：`core::subscriptions`（订阅）与 `core::sub_kernel`（内核）是唯一
 * 事实来源，HTTP 契约见 `api::subscriptions` 的模块头。
 *
 * ── 数据层在隔壁 ─────────────────────────────────────────────
 * 本文件只放视图层（内核状态行 + 订阅表 + 新增/编辑弹窗）；状态、取数、
 * 动作都在 `proxies-sub-state.ts`（普通模块，与 proxies-state.ts 同一拆分）。
 *
 * ── 内核命令是异步的（界面轮询）────────────────────────────
 * 写操作后端投一条内核命令就立即返回（内核重启 + 拉订阅要几秒到几十秒），
 * 这里在每次写操作后轮询内核状态几轮（见 proxies-sub-state 的
 * pollKernelAfterChange），节点数 / 运行状态随之刷新；下载内核的进度条
 * 随轮询的 received/total 变。
 *
 * ── 二维码导入 ──────────────────────────────────────────────
 * 桌面端没有摄像头：入口是「从图片识别」按钮（文件选择器）与弹窗内的
 * 粘贴事件（Ctrl+V 剪贴板里的截图）。解码在服务端（rqrr），这里只做
 * base64 上传与回填链接；识别到单节点分享链接时后端明确报「暂不支持」。
 *
 * ── 样式 ────────────────────────────────────────────────────
 * 面板骨架（.panel / .panel-head / .models-table）与代理池面板同一套全局
 * 样式；内核状态行用既有令牌（border-border / bg-surface-2 / text-subtle），
 * 不新增 CSS 文件。
 */

import * as React from 'react'
import {
  Badge,
  Button,
  Dialog,
  DialogBody,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  Input,
  Label,
  Progress,
  Switch,
} from '@ui'
import {
  addFileSubscription, addSubscription, decodeQrImage, downloadKernel, errorMessage, kernel,
  loadSection, pickSubscriptionFile, refreshSubscription, removeSubscription, replaceSubscriptionFile,
  restartKernel, saveSubscription, subs, subscribe, getSnapshot, toast, toggleSubscription, wb,
  type ImportedFile, type KernelStatus, type SubscriptionItem,
} from './proxies-sub-state'
import { formatTime } from './proxies-state'
import { t } from '../i18n'

/* ─── 订阅区块 ───────────────────────────────── */

export function SubscriptionSection() {
  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot)
  const items = subs()
  const kernelStatus = kernel()
  const ready = snapshot.data !== null
  const [modal, setModal] = React.useState<{ item: SubscriptionItem | null } | null>(null)

  // 首次渲染自拉一次：本区块没有像代理池那样的「页面加载契约」（wbProxiesPanel.load
  // 只刷池），不拉的话快照永远停在 null、界面停在「加载中…」
  React.useEffect(() => { void loadSection() }, [])

  const downloading = kernelStatus?.download.state === 'downloading'
  const downloadPercent =
    downloading && kernelStatus!.download.total > 0
      ? Math.round((kernelStatus!.download.received / kernelStatus!.download.total) * 100)
      : null

  return (
    <section className='panel'>
      <div className='panel-head'>
        <h2>{t('订阅')}</h2>
        <span className='panel-sub'>
          {ready ? t('{n} 条订阅', { n: items.length }) : t('加载中…')}
        </span>
        <div className='head-actions'>
          <Button variant='default' disabled={snapshot.pending}
            onClick={() => setModal({ item: null })}>{t('＋ 新增订阅')}</Button>
        </div>
      </div>

      <KernelRow kernel={kernelStatus} downloading={downloading || false}
        percent={downloadPercent} pending={snapshot.pending} />

      <div className='models-table-wrap'>
        <table className='models-table'>
          <colgroup>
            <col className='w-[18%]' />
            <col className='w-[8%]' />
            <col className='w-[32%]' />
            <col className='w-[9%]' />
            <col className='w-[13%]' />
            <col className='w-[8%]' />
            <col className='w-[12%]' />
          </colgroup>
          <thead>
            <tr>
              <th>{t('名称')}</th>
              <th className='ta-center'>{t('来源')}</th>
              <th>{t('订阅链接')}</th>
              <th className='ta-center'>{t('节点')}</th>
              <th>{t('上次同步')}</th>
              <th className='ta-center'>{t('启用')}</th>
              <th className='ta-right'>{t('操作')}</th>
            </tr>
          </thead>
          <tbody>
            {items.length ? items.map(item => {
              const sync = kernelStatus?.providerSync?.[item.id]
              const isFile = item.kind === 'file'
              return (
                <tr key={item.id} className={item.enabled ? '' : 'off'}>
                  <td><b>{item.name || t('未命名')}</b></td>
                  {/* 来源列：本地配置不参与定时刷新，这一点必须在表格里看得见 ——
                      否则用户会以为它跟链接订阅一样每天自动更新 */}
                  <td className='ta-center'>
                    {isFile
                      ? <Badge variant='secondary' title={t('本地配置文件：不参与定时刷新，改文件后在行内点「重新导入」')}>{t('本地文件')}</Badge>
                      : <Badge title={t('内核按 24 小时间隔拉取')}>{t('链接')}</Badge>}
                  </td>
                  {/* 链接列：本地配置显示原路径（title 里给完整路径），链接订阅显示主机段 */}
                  <td className='group'>
                    <div className='flex items-center gap-1'>
                      {isFile ? (
                        <>
                          <span className='font-mono text-[12px] text-text-2'
                            title={item.sourcePath || t('未记录原路径（网页端导入）')}>
                            {item.sourcePath ? shortPath(item.sourcePath) : t('未记录原路径')}
                          </span>
                          {item.sourcePath ? (
                            <Button variant='ghost' size='icon-xs'
                              className='copy-btn size-[21px] rounded-[var(--r-xs)] opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100'
                              data-copy={item.sourcePath} title={t('复制文件路径')}>⧉</Button>
                          ) : null}
                        </>
                      ) : (
                        <>
                          <span className='font-mono text-[12px] text-text-2'
                            title={item.url}>{shortUrl(item.url)}</span>
                          <Button variant='ghost' size='icon-xs'
                            className='copy-btn size-[21px] rounded-[var(--r-xs)] opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100'
                            data-copy={item.url} title={t('复制订阅链接')}>⧉</Button>
                        </>
                      )}
                    </div>
                  </td>
                  {/* 节点列只放读数；同步时间归「上次同步」列（两列各就各位）。
                      节点数优先取内核状态里的**实时**同步结果：条目上的 nodeCount
                      是列表响应注入的快照，写操作后的轮询窗口里可能还是旧的 0
                      （徽标曾冻在「—」上 —— 列表与内核一起轮询 + 这里的实时兜底，
                      双保险） */}
                  <td className='ta-center'>
                    {liveNodeCount(sync, item) > 0 ? (
                      <Badge variant='secondary'>{t('{n} 个', { n: liveNodeCount(sync, item) })}</Badge>
                    ) : (
                      <span className='text-subtle'>{t('—')}</span>
                    )}
                  </td>
                  <td>
                    {sync?.at ? (
                      <span className='text-[12px] text-text-2'>{formatTime(sync.at)}</span>
                    ) : (
                      <span className='text-subtle'>{t('未同步')}</span>
                    )}
                  </td>
                  <td className='ta-center'>
                    <Switch checked={item.enabled} disabled={snapshot.pending}
                      aria-label={t('启用订阅「{name}」', { name: item.name || t('未命名') })}
                      onCheckedChange={next => void toggleSubscription(item, next)} />
                  </td>
                  <td>
                    <div className='row-actions'>
                      <Button size='sm' variant='ghost' disabled={snapshot.pending || snapshot.refreshing !== null}
                        onClick={() => void refreshSubscription(item)}>
                        {snapshot.refreshing === item.id
                          ? t('更新中…')
                          : isFile ? t('重新导入') : t('立即更新')}
                      </Button>
                      {/* 换文件：本地文件订阅独有的入口（原地替换，id 不变）。
                          链接订阅没有这一项 —— 改链接在「编辑」里改。 */}
                      {isFile ? (
                        <Button size='sm' variant='ghost' disabled={snapshot.pending}
                          title={t('换一份配置文件替换当前内容（订阅 id 不变，账号引用不会断）')}
                          onClick={() => void replaceLocalFile(item)}>
                          {t('换文件')}
                        </Button>
                      ) : null}
                      <Button size='sm' variant='ghost' disabled={snapshot.pending}
                        onClick={() => setModal({ item })}>{t('编辑')}</Button>
                      <Button size='sm' variant='destructive' disabled={snapshot.pending}
                        onClick={() => void removeSubscription(item)}>{t('删除')}</Button>
                    </div>
                  </td>
                </tr>
              )
            }) : (
              <tr>
                <td colSpan={7} className='empty'>
                  {ready ? (
                    /* 空态内嵌主操作按钮（视线不用跳到右上角）：一句话 + 按钮 +
                       一句「接下来会发生什么」的副提示。本地配置导入在弹窗里
                       （它要一起填名字），空态只指向弹窗那一个入口。 */
                    <div className='flex flex-col items-center gap-2 py-3'>
                      <span>{t('还没有订阅：粘贴订阅链接、从图片识别二维码，或导入本机已有的 Clash / mihomo 配置文件。')}</span>
                      <Button size='sm' variant='outline' disabled={snapshot.pending}
                        onClick={() => setModal({ item: null })}>{t('＋ 新增订阅')}</Button>
                      <span className='text-[11px] text-subtle'>
                        {t('订阅里的节点会自动同步进下方代理池，账号即可引用。')}
                      </span>
                    </div>
                  ) : t('加载中…')}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      <div className='panel-foot'>
        <span>
          {t('订阅里的节点会被内核起成本地监听器，镜像到下方代理池（账号按 id 引用，与本页其它出口同一用法）。内核只在用到订阅时按需下载（约 20-40MB），不随安装包分发。')}
        </span>
        <span>
          {t('两种来源的刷新方式不同：链接订阅由内核每 24 小时自动拉取；本地配置文件不参与定时刷新，改完文件在行内点「重新导入」即可；换成另一份文件点「换文件」（原地替换，订阅 id 不变，账号引用不会断）。')}
        </span>
        {snapshot.error ? (
          <span className='text-destructive'>{t('读取失败：{reason}', { reason: snapshot.error })}</span>
        ) : null}
      </div>

      {modal ? (
        <SubModal item={modal.item} onClose={() => setModal(null)} />
      ) : null}
    </section>
  )
}

/* ─── 内核状态行 ─────────────────────────────── */

function KernelRow({
  kernel, downloading, percent, pending,
}: {
  kernel: KernelStatus | null
  downloading: boolean
  percent: number | null
  pending: boolean
}) {
  return (
    /* 紧凑状态条：一行放下徽标 / 读数 / 操作，进度条只在下载中展开；
       未安装是个高频空态，说明文字收在面板页脚，这里只留一行短文案 */
    <div className='mx-3 mb-3 flex flex-col gap-2 rounded-md border border-border bg-surface-2 px-3 py-2'>
      <div className='flex flex-wrap items-center gap-2'>
        <Badge variant={kernel?.running ? 'default' : 'secondary'}>
          {kernel?.running
            ? t('内核运行中')
            : kernel?.installed
              ? t('内核已停止')
              : t('内核未安装')}
        </Badge>
        <span className='text-[12.5px] text-text-2'>
          {/* 版本号是 t 的形参（占位符不传参会原样显示 "{version}"）；
              未安装时不显示版本段，只留按需下载的说明 */}
          {kernel?.installed
            ? t('内核 {version}', { version: kernel.version }) + '　·　'
            : ''}
          {kernel?.running
            ? t('控制端口 {port} · {n} 个节点监听器', { port: kernel?.controllerPort ?? '—', n: kernel?.listenerCount ?? 0 })
            : kernel?.installed
              ? t('内核已停止，点「启动内核」拉起')
              : t('按需下载（约 20-40MB）')}
        </span>
        <div className='ml-auto'>
          {!kernel?.installed ? (
            <Button size='sm' variant='outline' disabled={pending || downloading}
              onClick={() => void downloadKernel()}>
              {downloading ? t('下载中…') : t('下载内核')}
            </Button>
          ) : (
            <Button size='sm' variant='outline' disabled={pending || downloading}
              onClick={() => void restartKernel()}>
              {kernel?.running ? t('重启内核') : t('启动内核')}
            </Button>
          )}
        </div>
      </div>
      {downloading ? (
        <div className='flex items-center gap-3'>
          <Progress className='min-w-[200px] flex-1' value={percent ?? null} />
          <span className='text-[11px] text-subtle'>
            {percent !== null
              ? t('{percent}%（{received} / {total} MB）', {
                  percent,
                  received: mb(kernel!.download.received),
                  total: mb(kernel!.download.total),
                })
              : t('下载中…')}
          </span>
        </div>
      ) : null}
      {kernel?.lastError ? (
        <span className='text-[12px] text-destructive'>{kernel.lastError}</span>
      ) : null}
    </div>
  )
}

/** 字节 → MB 文案（下载进度用；total 未知时是 —） */
function mb(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '—'
  return (value / 1000 / 1000).toFixed(1)
}

/** 链接截断（表格里只显示主机段；title 里有完整链接） */
function shortUrl(url: string): string {
  try {
    const parsed = new URL(url)
    return parsed.host + (parsed.pathname !== '/' ? '/…' : '')
  } catch {
    return url.slice(0, 60)
  }
}

/** 路径截断（只留末两段；title 里有完整路径） */
function shortPath(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts.length <= 2 ? path : `…/${parts.slice(-2).join('/')}`
}

/**
 * 节点数：优先内核状态里的**实时**同步结果，回落条目上的注入快照。
 *
 * 两者同源（条目上的 nodeCount 由后端在列表响应里从 providerSync 注入），
 * 但时序不同：写操作后的轮询窗口里，条目快照可能还是操作前的 0，而内核
 * 状态已经拿到新结果 —— 徽标曾因此冻在「—」上，这里以实时值兜底。
 */
function liveNodeCount(
  sync: { count?: number } | undefined,
  item: SubscriptionItem,
): number {
  return sync?.count ?? item.nodeCount
}

/**
 * 换一份本地配置文件：弹选文件框 → 直接原地替换（不改名字、不重建条目）。
 *
 * 为什么不进「编辑」弹窗：那个弹窗的职责是改名字 / 启用位，而换文件是一次
 * 「选文件 + 校验 + 写盘 + 让内核重读」的动作 —— 塞进弹窗会让用户以为要再点
 * 一次保存，也会把「取消弹窗」与「取消换文件」两种意图混在一起。
 * 失败不改变任何状态（后端先校验后写盘，见 `api::subscriptions::replace_file`）。
 */
async function replaceLocalFile(item: SubscriptionItem): Promise<void> {
  let picked: ImportedFile | null
  try {
    picked = await pickSubscriptionFile()
  } catch (error) {
    toast(t('读取文件失败：{reason}', { reason: errorMessage(error) }), 'err')
    return
  }
  if (!picked) return
  try {
    await replaceSubscriptionFile(item, picked.content || '', picked.path || undefined)
  } catch (error) {
    toast(t('替换失败：{reason}', { reason: errorMessage(error) }), 'err')
  }
}

/* ─── 新增 / 编辑弹窗 ───────────────────────── */

function SubModal({
  item,
  onClose,
}: {
  item: SubscriptionItem | null
  onClose: () => void
}) {
  const editing = item !== null
  const [name, setName] = React.useState(item?.name || '')
  const [url, setUrl] = React.useState(item?.url || '')
  const [enabled, setEnabled] = React.useState(item ? item.enabled : true)
  const [saving, setSaving] = React.useState(false)
  const [decoding, setDecoding] = React.useState(false)
  const [status, setStatus] = React.useState<React.ReactNode>(null)
  const fileInput = React.useRef<HTMLInputElement | null>(null)
  /**
   * 导入的本地配置文件（新增时的第二种来源）。
   *
   * 只有**新增**能选它：编辑一条已有的链接订阅时改成「本地文件」需要重新选
   * 文件、重建 provider，与「改个名字 / 链接」不是同一件事 —— 那种情况让用户
   * 删掉重建。编辑一条本地文件订阅时这里固定显示它的原路径（只读）。
   */
  const [localFile, setLocalFile] = React.useState<ImportedFile | null>(null)
  const isFileMode = editing ? item!.kind === 'file' : localFile !== null

  async function save(): Promise<void> {
    if (saving) return
    const nameText = name.trim()
    if (!nameText) {
      setStatus(<span className='text-destructive'>❌ {t('请填写订阅名称')}</span>)
      return
    }
    if (editing) {
      // 本地文件订阅的编辑只改名字 / 启用位：来源与内容都不在这里动
      //（要换文件就删掉重建，见 localFile 的说明）。链接订阅则要求合法链接。
      if (isFileMode) {
        setSaving(true)
        try {
          await saveSubscription(item!, { name: nameText, url: item!.url, enabled })
          onClose()
        } catch (error) {
          setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
          setSaving(false)
        }
        return
      }
      const urlText = url.trim()
      if (!(urlText.startsWith('http://') || urlText.startsWith('https://'))) {
        setStatus(<span className='text-destructive'>❌ {t('订阅链接必须是 http:// 或 https:// 开头')}</span>)
        return
      }
      setSaving(true)
      try {
        await saveSubscription(item!, { name: nameText, url: urlText, enabled })
        onClose()
      } catch (error) {
        setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
        setSaving(false)
      }
      return
    }
    if (localFile) {
      // 本地配置：内容已经拿到（选文件时由壳读好、后端校验过），这里只提交
      setSaving(true)
      try {
        await addFileSubscription(nameText, localFile.path || '', localFile.content || '')
        onClose()
      } catch (error) {
        setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
        setSaving(false)
      }
      return
    }
    const urlText = url.trim()
    if (!(urlText.startsWith('http://') || urlText.startsWith('https://'))) {
      setStatus(<span className='text-destructive'>❌ {t('订阅链接必须是 http:// 或 https:// 开头')}</span>)
      return
    }
    setSaving(true)
    try {
      await addSubscription(nameText, urlText)
      onClose()
    } catch (error) {
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
      setSaving(false)
    }
  }

  /** 选本地配置文件：壳弹系统选文件框 → 后端校验并回执元信息 */
  async function pickFile(): Promise<void> {
    if (saving) return
    setStatus(t('正在读取文件…'))
    try {
      const picked = await pickSubscriptionFile()
      if (!picked) {
        setStatus(null)
        return
      }
      setLocalFile(picked)
      if (!name.trim() && picked.name) setName(picked.name)
      setStatus(
        <span className='text-success'>
          ✅ {t('已读取：{n} 条节点声明', { n: picked.declaredNodes ?? 0 })}
        </span>,
      )
    } catch (error) {
      setLocalFile(null)
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
    }
  }

  /** 从图片文件解二维码 → 回填链接（服务端解码，见 proxies-sub-state） */
  async function decodeFrom(file: File): Promise<void> {
    if (decoding) return
    setDecoding(true)
    setStatus(t('正在识别二维码…'))
    try {
      const decoded = await decodeQrImage(file)
      setUrl(decoded)
      setStatus(<span className='text-success'>✅ {t('已识别订阅链接')}</span>)
    } catch (error) {
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
    } finally {
      setDecoding(false)
    }
  }

  /** 剪贴板里是图片（截图的二维码）时直接识别：粘贴事件里取文件 */
  function onPaste(event: React.ClipboardEvent): void {
    const files: File[] = Array.from(event.clipboardData?.files || [])
    const image = files.find(file => file.type.startsWith('image/'))
    if (image) {
      event.preventDefault()
      void decodeFrom(image)
    }
  }

  return (
    <Dialog open onOpenChange={open => { if (!open && !saving) onClose() }}>
      <DialogContent className='max-w-[560px]' onPaste={onPaste}>
        <DialogHeader>
          <DialogTitle>{editing ? t('编辑订阅') : t('新增订阅')}</DialogTitle>
        </DialogHeader>
        <DialogBody className='flex flex-col gap-3.5'>
          <div className='flex flex-col gap-1.5'>
            <Label htmlFor='sub-name'>{t('名称')}</Label>
            <Input id='sub-name' maxLength={60} autoComplete='off' spellCheck={false}
              placeholder={t('例如：我的机场')} value={name}
              onChange={event => setName(event.currentTarget.value)} />
          </div>

          <div className='flex flex-col gap-1.5'>
            <Label htmlFor='sub-url'>{isFileMode ? t('配置文件') : t('订阅链接')}</Label>
            {isFileMode ? (
              <div className='flex items-center gap-2'>
                <Input id='sub-url' readOnly spellCheck={false}
                  value={localFile?.path || item?.sourcePath || ''}
                  placeholder={t('（网页端导入：不记录原路径）')} />
                {editing ? null : (
                  <Button size='sm' variant='outline' disabled={saving}
                    onClick={() => void pickFile()}>{t('重新选择')}</Button>
                )}
              </div>
            ) : (
              <Input id='sub-url' autoComplete='off' spellCheck={false}
                placeholder='https://…' value={url}
                onChange={event => setUrl(event.currentTarget.value)} />
            )}
            {isFileMode ? null : (
              <div className='flex items-center gap-2'>
                <Button size='sm' variant='outline' disabled={decoding}
                  onClick={() => fileInput.current?.click()}>
                  {decoding ? t('识别中…') : t('从图片识别二维码')}
                </Button>
                <Button size='sm' variant='outline' disabled={saving}
                  onClick={() => void pickFile()}>{t('导入本地配置文件')}</Button>
                <span className='text-[11px] text-subtle'>{t('也可以把二维码截图直接粘贴到弹窗里（Ctrl+V）')}</span>                <input ref={fileInput} type='file' accept='image/*' className='hidden'
                  onChange={event => {
                    const file = event.currentTarget.files?.[0]
                    if (file) void decodeFrom(file)
                    event.currentTarget.value = ''
                  }} />
              </div>
            )}
          </div>

          <div className='flex flex-row items-center gap-2.5'>
            <Switch checked={enabled} onCheckedChange={setEnabled} aria-label={t('启用这条订阅')} />
            <span className='text-[12.5px]'>{t('启用（禁用的订阅不参与内核同步，它的节点会从代理池移除）')}</span>
          </div>

          <p className='text-xs leading-[1.65] text-subtle'>
            {isFileMode
              ? t('本地配置文件只认 Clash / mihomo 的 YAML（含 proxies 列表）。它不参与定时刷新：改完原文件后，在列表里点「重新导入」。')
              : t('订阅内容由内核定时刷新（每 24 小时）；节点数与同步状态在保存后由轮询刷新，稍等几秒。')}
          </p>

          <div className='min-h-[18px] text-xs text-subtle'>{status}</div>
        </DialogBody>
        <DialogFooter>
          <div className='mr-auto' />
          <Button variant='outline' disabled={saving} onClick={onClose}>{t('取消')}</Button>
          <Button variant='default' disabled={saving || decoding || (isFileMode && !editing && !localFile)}
            onClick={() => void save()}>
            {saving ? t('保存中…') : t('保存')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

import * as React from 'react'
import {
  Button,
  Checkbox,
  Dialog,
  DialogBody,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  Input,
  Label,
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
  Switch,
} from '@ui'
import {
  durationText, errorMessage, loadSettings, saveItem, saveSettings, toast, wb,
  type ProxyPoolItem, type ProxySettings, type ProxySettingsPayload,
} from './proxies-state'
import { t } from '../i18n'
// 赞助位外链：与文档页/更新日志同一条路（data-external 委托 + 系统浏览器打开）。
// 链接本体在 sponsor.ts（唯一定义点），这里只取用。
import { handleExternalClick } from './update-shared'
import { PROXYLANE_URL } from './sponsor'

/* ─── 新增 / 编辑弹窗 ───────────────────────── */

/**
 * 赞助位里的行内链接：一处样式、一条链接（同一行的品牌名与「去获取」都用它）。
 * 「链接只裹需要点的字」是全站赞助位的口径 —— 句子按片段拼，翻译时片段可以
 * 各自贴合目标语言的语序。
 */
function SponsorLink({ children }: { children: React.ReactNode }) {
  return (
    <a
      className='font-semibold text-primary-fg no-underline hover:underline'
      href={PROXYLANE_URL}
      data-external={PROXYLANE_URL}
      onClick={handleExternalClick}
    >{children}</a>
  )
}

export function ProxyModal({
  item,
  onClose,
}: {
  item: ProxyPoolItem | null
  onClose: () => void
}) {
  const editing = item !== null
  const [name, setName] = React.useState(item?.name || '')
  const [protocol, setProtocol] = React.useState<'http' | 'socks5'>(item?.protocol === 'socks5' ? 'socks5' : 'http')
  const [host, setHost] = React.useState(item?.host || '')
  const [port, setPort] = React.useState(item?.port ? String(item.port) : '')
  const [username, setUsername] = React.useState(item?.username || '')
  const [password, setPassword] = React.useState(item?.password || '')
  const [enabled, setEnabled] = React.useState(item ? item.enabled : true)
  const [useAuth, setUseAuth] = React.useState(Boolean(item?.username || item?.password))
  const [saving, setSaving] = React.useState(false)
  const [testing, setTesting] = React.useState(false)
  const [status, setStatus] = React.useState<React.ReactNode>(null)

  /** 表单 → 归一化后的提交对象；形状非法时返回一句面向用户的错误 */
  function draftPayload(): { payload: Record<string, unknown> } | { error: string } {
    const trimmed = name.trim()
    if (!trimmed) return { error: t('请填写代理名称') }
    const hostText = host.trim()
    if (!hostText) return { error: t('请填写代理主机地址') }
    const portNumber = Number(port)
    if (!Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535) {
      return { error: t('代理端口必须是 1-65535 的整数') }
    }
    return {
      payload: {
        name: trimmed,
        source: 'manual',
        protocol,
        host: hostText,
        port: portNumber,
        username: useAuth ? username.trim() : '',
        password: useAuth ? password : '',
        enabled,
      },
    }
  }

  /** 用当前表单内容测一次（**未保存也能测**：先确认能通，再决定保存） */
  async function testDraft(): Promise<void> {
    if (testing) return
    const draft = draftPayload()
    if ('error' in draft) {
      setStatus(<span className='text-destructive'>❌ {draft.error}</span>)
      return
    }
    const payload = draft.payload
    const proxy = {
      source: 'custom',
      protocol: payload.protocol,
      host: payload.host,
      port: payload.port,
      username: payload.username,
      password: payload.password,
    }
    setTesting(true)
    setStatus(t('正在连接上游…'))
    try {
      const data = await wb().workbuddyDesktop?.testProxy?.({ proxy })
      if (data?.success) {
        const suffix = durationText(data.durationMs)
        setStatus(
          <span className='text-success'>
            {t('✅ 出口可用')}
            {data.ip ? t('　出口 IP {ip}', { ip: data.ip }) : ''}
            {suffix ? `　${suffix}` : ''}
          </span>,
        )
      } else {
        setStatus(<span className='text-destructive'>❌ {data?.error || t('连接失败')}</span>)
      }
    } catch (error) {
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
    } finally {
      setTesting(false)
    }
  }

  async function save(): Promise<void> {
    if (saving) return
    const draft = draftPayload()
    if ('error' in draft) {
      setStatus(<span className='text-destructive'>❌ {draft.error}</span>)
      return
    }
    setSaving(true)
    setStatus(null)
    try {
      await saveItem(item, draft.payload)
      toast(editing ? t('已保存「{name}」', { name: name.trim() }) : t('已新增「{name}」', { name: name.trim() }))
      onClose()
    } catch (error) {
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={open => { if (!open && !saving) onClose() }}>
      <DialogContent className='max-w-[560px]'>
        <DialogHeader>
          <DialogTitle>{editing ? t('编辑代理') : t('新增代理')}</DialogTitle>
        </DialogHeader>
        <DialogBody className='flex flex-col gap-3.5'>
          <div className='flex flex-col gap-1.5'>
            <Label htmlFor='proxy-name'>{t('名称')}</Label>
            <Input id='proxy-name' maxLength={60} autoComplete='off' spellCheck={false}
              placeholder={t('例如：香港 HTTP 出口')}
              value={name} onChange={event => setName(event.currentTarget.value)} />
          </div>

          <div className='flex flex-row items-end gap-2.5'>
            <div className='flex flex-col gap-1.5'>
              <Label htmlFor='proxy-protocol'>{t('协议')}</Label>
              {/* 协议名 HTTP / SOCKS5 是固定叫法，不进词典 */}
              <Select value={protocol} onValueChange={value => setProtocol(value as 'http' | 'socks5')}>
                <SelectTrigger id='proxy-protocol' className='min-w-[110px]'>
                  <SelectValue>{protocol === 'socks5' ? 'SOCKS5' : 'HTTP'}</SelectValue>
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value='http'>HTTP</SelectItem>
                  <SelectItem value='socks5'>SOCKS5</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className='flex min-w-0 flex-1 flex-col gap-1.5'>
              <Label htmlFor='proxy-host'>{t('主机')}</Label>
              <Input id='proxy-host' placeholder='127.0.0.1' autoComplete='off' spellCheck={false}
                value={host} onChange={event => setHost(event.currentTarget.value)} />
            </div>
            <div className='flex flex-col gap-1.5'>
              <Label htmlFor='proxy-port'>{t('端口')}</Label>
              <Input id='proxy-port' type='number' min={1} max={65535} className='max-w-[110px]'
                placeholder='7890' value={port} onChange={event => setPort(event.currentTarget.value)} />
            </div>
          </div>
          <p className='text-xs leading-[1.8] text-subtle'>{t('地址由网关所在设备访问；本机代理需运行在该设备上。')}</p>
          <Label className='flex cursor-pointer items-center gap-2 text-xs font-normal'>
            <Checkbox checked={useAuth} onCheckedChange={value => setUseAuth(value === true)} />
            {t('需要用户名和密码（可选）')}
          </Label>
          {useAuth ? <div className='flex flex-row items-end gap-2.5'>
            <div className='flex min-w-0 flex-1 flex-col gap-1.5'>
              <Label htmlFor='proxy-user'>{t('用户名')}</Label>
              <Input id='proxy-user' placeholder={t('可选')} autoComplete='off'
                value={username} onChange={event => setUsername(event.currentTarget.value)} />
            </div>
            <div className='flex min-w-0 flex-1 flex-col gap-1.5'>
              <Label htmlFor='proxy-pass'>{t('密码')}</Label>
              <Input id='proxy-pass' type='password' placeholder={t('可选')} autoComplete='new-password'
                value={password} onChange={event => setPassword(event.currentTarget.value)} />
            </div>
          </div> : null}

          <div className='flex flex-row items-center gap-2.5'>
            <Switch checked={enabled} onCheckedChange={setEnabled} aria-label={t('启用这条代理')} />
            <span className='text-[12.5px]'>
              {t('启用（禁用的条目不会被账号使用，被引用时账号回退直连；仍可测试）')}
            </span>
          </div>

          {/* 赞助位：一行提示（另两处在 README 顶部与「文档」页的网络代理示例）。
              品牌名与句尾的「去获取」都是行内链接（SponsorLink），翻译时片段
              可以各自贴合目标语言的语序 */}
          <p className='text-xs leading-[1.65] text-subtle'>
            {t('需要干净的出口？')}
            <SponsorLink>ProxyLane</SponsorLink>
            {t(' AI 账号专用静态住宅 IP：Scamalytics 低欺诈评分，不限流量，首单 75 折（AGENT2API25）')}
            {' '}
            <SponsorLink>{t('去获取')}</SponsorLink>
          </p>

          <div className='min-h-[18px] text-xs text-subtle'>{status}</div>
        </DialogBody>
        <DialogFooter>
          <Button variant='outline' disabled={testing || saving} onClick={() => void testDraft()}>
            {testing ? t('测试中…') : t('测试出口')}
          </Button>
          <div className='mr-auto' />
          <Button variant='outline' disabled={saving} onClick={onClose}>{t('取消')}</Button>
          <Button variant='default' disabled={saving} onClick={() => void save()}>
            {saving ? t('保存中…') : t('保存')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/* ─── 网络代理设置 ─────────────────────────── */

/**
 * 「网络代理 → 设置」弹窗：节点端口区间 + 出口测试网址 + 轮询出口的并发预算。
 *
 * ── 三组字段各自的生效时机不同（保存后要说清楚）─────────────────
 *   · **端口区间**：留给订阅节点的本地监听端口段。内核给被引用的节点开监听器
 *     时从这段里挑空闲端口，**要手动重启内核**才按新范围分配；已分配的端口
 *     不会因为改范围而失效（否则所有账号引用会一起断）。
 *   · **测试网址**：下一次「测试」立刻生效。
 *   · **并发 / 排队**：下一个请求立刻生效。
 *   所以保存后**不自动关窗** —— 那段「端口还没落地」的话要留在眼前。
 */
export function ProxySettingsModal({ onClose }: { onClose: () => void }) {
  const [loading, setLoading] = React.useState(true)
  const [saving, setSaving] = React.useState(false)
  const [base, setBase] = React.useState('')
  const [end, setEnd] = React.useState('')
  const [testUrl, setTestUrl] = React.useState('')
  const [maxConcurrentPerIp, setMaxConcurrentPerIp] = React.useState('1')
  const [maxQueueSize, setMaxQueueSize] = React.useState('64')
  const [queueTimeoutSeconds, setQueueTimeoutSeconds] = React.useState('30')
  const [defaults, setDefaults] = React.useState<NonNullable<ProxySettings['defaults']>>({})
  const [error, setError] = React.useState('')
  /** 保存成功后说明并发预算与端口范围的不同生效时机。 */
  const [saved, setSaved] = React.useState(false)

  React.useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const data = await loadSettings()
        if (!alive || data === null) return
        setBase(String(data.listenerPortBase ?? ''))
        setEnd(String(data.listenerPortEnd ?? ''))
        setTestUrl(String(data.testUrl ?? ''))
        setMaxConcurrentPerIp(String(data.rotateMaxConcurrentPerIp ?? data.defaults?.rotateMaxConcurrentPerIp ?? 1))
        setMaxQueueSize(String(data.rotateMaxQueueSize ?? data.defaults?.rotateMaxQueueSize ?? 64))
        setQueueTimeoutSeconds(String(data.rotateQueueTimeoutSeconds ?? data.defaults?.rotateQueueTimeoutSeconds ?? 30))
        setDefaults(data.defaults ?? {})
        setLoading(false)
      } catch (cause) {
        if (!alive) return
        setError(errorMessage(cause))
        setLoading(false)
      }
    })()
    return () => { alive = false }
  }, [])

  /** 表单 → 提交 payload；形状非法时返回 null（错误写进状态行） */
  function draftPayload(): ProxySettingsPayload | null {
    const baseNumber = Number(base)
    const endNumber = Number(end)
    // 下界 1024 与后端同一口径（低于它是系统保留段，后端会拒）
    if (!Number.isInteger(baseNumber) || baseNumber < 1024 || baseNumber > 65535
      || !Number.isInteger(endNumber) || endNumber < 1024 || endNumber > 65535) {
      setError(t('端口范围必须是 1024-65535 的整数'))
      return null
    }
    if (baseNumber > endNumber) {
      setError(t('起始端口不能大于结束端口'))
      return null
    }
    const url = testUrl.trim()
    if (url && !/^https?:\/\//i.test(url)) {
      setError(t('测试网址要以 http:// 或 https:// 开头'))
      return null
    }
    const concurrent = Number(maxConcurrentPerIp)
    const queue = Number(maxQueueSize)
    const timeout = Number(queueTimeoutSeconds)
    // 空串不能隐式变成 0；只收十进制整数字面量，拒绝小数、NaN、指数及超限
    if (!/^\d+$/.test(maxConcurrentPerIp.trim()) || !Number.isInteger(concurrent)
      || concurrent < 1 || concurrent > 32) {
      setError(t('每个出口 IP 的最大并发必须是 1-32 的整数'))
      return null
    }
    if (!/^\d+$/.test(maxQueueSize.trim()) || !Number.isInteger(queue) || queue < 0 || queue > 1024) {
      setError(t('最大排队数必须是 0-1024 的整数'))
      return null
    }
    if (!/^\d+$/.test(queueTimeoutSeconds.trim()) || !Number.isInteger(timeout)
      || timeout < 1 || timeout > 300) {
      setError(t('排队超时必须是 1-300 秒的整数'))
      return null
    }
    setError('')
    return {
      listenerPortBase: baseNumber,
      listenerPortEnd: endNumber,
      testUrl: url,
      rotateMaxConcurrentPerIp: concurrent,
      rotateMaxQueueSize: queue,
      rotateQueueTimeoutSeconds: timeout,
    }
  }

  async function save(): Promise<void> {
    if (saving || loading) return
    setSaved(false)
    const payload = draftPayload()
    if (!payload) return
    setSaving(true)
    try {
      await saveSettings(payload)
      setSaved(true)
      toast(t('✅ 网络代理设置已保存'))
    } catch (cause) {
      setError(errorMessage(cause))
    } finally {
      setSaving(false)
    }
  }

  /** 恢复默认：把默认值回填进表单（还不是「保存」—— 用户确认后一起提交） */
  function resetToDefaults(): void {
    setBase(String(defaults.listenerPortBase ?? ''))
    setEnd(String(defaults.listenerPortEnd ?? ''))
    setTestUrl(String(defaults.testUrl ?? ''))
    setMaxConcurrentPerIp(String(defaults.rotateMaxConcurrentPerIp ?? 1))
    setMaxQueueSize(String(defaults.rotateMaxQueueSize ?? 64))
    setQueueTimeoutSeconds(String(defaults.rotateQueueTimeoutSeconds ?? 30))
    setSaved(false)
    setError('')
  }

  return (
    <Dialog open onOpenChange={open => { if (!open && !saving) onClose() }}>
      <DialogContent className='max-w-[560px]'>
        <DialogHeader>
          <DialogTitle>{t('代理设置')}</DialogTitle>
        </DialogHeader>
        <DialogBody className='flex flex-col gap-3.5'>
          {loading ? (
            <p className='text-xs text-subtle'>{t('加载中…')}</p>
          ) : (
            <>
              <div className='flex flex-row items-end gap-2.5'>
                <div className='flex min-w-0 flex-1 flex-col gap-1.5'>
                  <Label htmlFor='proxy-settings-base'>{t('节点端口范围（起始）')}</Label>
                  <Input id='proxy-settings-base' type='number' min={1024} max={65535}
                    placeholder={String(defaults.listenerPortBase ?? '')} autoComplete='off'
                    value={base} onChange={event => { setBase(event.currentTarget.value); setSaved(false) }} />
                </div>
                <span aria-hidden='true' className='flex-none pb-[7px] text-subtle'>—</span>
                <div className='flex min-w-0 flex-1 flex-col gap-1.5'>
                  <Label htmlFor='proxy-settings-end'>{t('节点端口范围（结束）')}</Label>
                  <Input id='proxy-settings-end' type='number' min={1024} max={65535}
                    placeholder={String(defaults.listenerPortEnd ?? '')} autoComplete='off'
                    value={end} onChange={event => { setEnd(event.currentTarget.value); setSaved(false) }} />
                </div>
              </div>
              <p className='text-xs leading-[1.65] text-subtle'>
                {t('订阅节点会在本机各占一个监听端口，从该范围里挑空闲的分配；节点多的大订阅把默认范围（{base}-{end}）用尽时会报「没有可用的节点端口」，这时把范围调大再重启内核。已分配出去的端口不会因改范围而失效。', {
                  base: defaults.listenerPortBase ?? '',
                  end: defaults.listenerPortEnd ?? '',
                })}
              </p>

              <div className='flex flex-col gap-1.5'>
                <Label htmlFor='proxy-settings-url'>{t('出口测试网址')}</Label>
                <Input id='proxy-settings-url' placeholder={defaults.testUrl || ''}
                  autoComplete='off' spellCheck={false} className='font-mono text-[12px]'
                  value={testUrl} onChange={event => { setTestUrl(event.currentTarget.value); setSaved(false) }} />
              </div>
              <p className='text-xs leading-[1.65] text-subtle'>
                {t('「测试」与「批量测试」访问的目标；留空用默认。判据是能否拿到 HTTP 响应，不用考虑该网址返回什么内容。')}
              </p>

              <div className='grid grid-cols-1 gap-2.5 sm:grid-cols-3'>
                <div className='flex min-w-0 flex-col gap-1.5'>
                  <Label htmlFor='proxy-settings-concurrent'>{t('每个出口 IP 的最大并发')}</Label>
                  <Input id='proxy-settings-concurrent' type='number' min={1} max={32} step={1}
                    placeholder={String(defaults.rotateMaxConcurrentPerIp ?? 1)} autoComplete='off'
                    value={maxConcurrentPerIp}
                    onChange={event => { setMaxConcurrentPerIp(event.currentTarget.value); setSaved(false) }} />
                </div>
                <div className='flex min-w-0 flex-col gap-1.5'>
                  <Label htmlFor='proxy-settings-queue'>{t('最大排队数')}</Label>
                  <Input id='proxy-settings-queue' type='number' min={0} max={1024} step={1}
                    placeholder={String(defaults.rotateMaxQueueSize ?? 64)} autoComplete='off'
                    value={maxQueueSize}
                    onChange={event => { setMaxQueueSize(event.currentTarget.value); setSaved(false) }} />
                </div>
                <div className='flex min-w-0 flex-col gap-1.5'>
                  <Label htmlFor='proxy-settings-queue-timeout'>{t('排队超时（秒）')}</Label>
                  <Input id='proxy-settings-queue-timeout' type='number' min={1} max={300} step={1}
                    placeholder={String(defaults.rotateQueueTimeoutSeconds ?? 30)} autoComplete='off'
                    value={queueTimeoutSeconds}
                    onChange={event => { setQueueTimeoutSeconds(event.currentTarget.value); setSaved(false) }} />
                </div>
              </div>
              <p className='text-xs leading-[1.65] text-subtle'>
                {t('仅在账号使用「轮询」档时生效：同一真实出口 IP 跨账号共享并发名额，响应流结束后释放；未知 IP 保守串行。全部繁忙时排队，不惩罚账号；最大排队数为 0 时满载立即拒绝。')}
              </p>

              <div className='min-h-[18px] text-xs'>
                {error ? <span className='text-destructive'>❌ {error}</span> : null}
                {!error && saved ? (
                  <span className='text-success'>
                    {t('已保存。并发与排队设置对下个请求生效；端口范围需手动重启内核重建监听器后用于新分配，已分配端口沿用。本次保存不会自动重启。')}
                  </span>
                ) : null}
              </div>
            </>
          )}
        </DialogBody>
        <DialogFooter>
          <Button variant='ghost' disabled={loading || saving} onClick={resetToDefaults}>{t('恢复默认')}</Button>
          <div className='mr-auto' />
          <Button variant='outline' disabled={saving} onClick={onClose}>{t('关闭')}</Button>
          <Button disabled={loading || saving} onClick={() => void save()}>{saving ? t('保存中…') : t('保存')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

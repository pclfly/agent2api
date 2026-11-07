/** 账号设置与批量操作共用的出网代理表单。 */

import * as React from 'react'
import {
  Button, Checkbox, Input, Label, MultiSelect, SegmentedControl,
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
  Field, FieldGroup, FieldLabel,
} from '@ui'
import { t } from '../i18n'
import {
  errorMessage, esc, poolItemLabel, poolItemSelectable, shared, toast,
  type AccountRecord, type PoolItem,
} from './accounts-shared'
import { proxyPoolOptions } from './accounts-data'
import { DIALOG_SEG_CLASS } from './dialog-segmented'
// 轮询档的形状读写（与后端归一逐字段对齐；纯函数，可被 npm test 覆盖）
import { rotateDraftOf, rotatePayloadOf } from './proxy-rotate-config'
import type { ClashProxySnapshot } from '../../../ui-shared/clash-proxy'

/** 每条候选都会真实出网，一轮最多测 8 条。 */
const ROTATE_TEST_LIMIT = 8

export type ProxyPayload =
  | null
  | { source: 'pool'; proxyId: string }
  | { source: 'rotate'; all: boolean; proxyIds?: string[]; failureMode?: 'smart' | 'all_errors' }
  | { source: 'clash'; listenerUid: string }
  | { source: 'custom'; protocol: 'http' | 'socks5'; host: string; port: number; username: string; password: string }

export type ProxyDraft = {
  mode: 'none' | 'pool' | 'rotate' | 'clash' | 'custom' | 'retired'
  listenerUid: string
  /** 池条目 id（mode === 'pool'） */
  proxyId: string
  /** 轮询档：true = 池里全部启用的出口（"全部"，默认档；false = 只在 rotateIds 里轮） */
  rotateAll: boolean
  /** 轮询档：指定要轮询的池条目 id（rotateAll 为 true 时不用） */
  rotateIds: string[]
  /**
   * 轮询档：什么失败才换出口。
   *   · `smart`（缺省）—— 只换与出口有关的（网络失败、403 地区封锁、按 IP 计的
   *     429）；业务错误（模型不存在、参数不对）换出口照样失败，不换；
   *   · `all_errors` —— 任何上游错误都换。
   */
  failureMode: 'smart' | 'all_errors'
  protocol: 'http' | 'socks5'
  host: string
  port: string
  username: string
  password: string
}

/** 账号 proxy 字段 → 表单草稿（形态与后端 workbuddy-proxy.mjs 一致） */
export function draftOfProxy(proxy: AccountRecord['proxy']): ProxyDraft {
  const source = proxy?.config?.source || proxy?.source
  const config = proxy?.config || null
  const rotate = source === 'rotate'
  return {
    mode: source === 'pool' ? 'pool'
      : rotate ? 'rotate'
        : source === 'clash' ? 'clash' : source === 'custom' ? 'custom' : source === 'subscription' ? 'retired' : 'none',
    listenerUid: source === 'clash' ? String(config?.listenerUid || '') : '',
    proxyId: source === 'pool' ? String(config?.proxyId || '') : '',
    // 轮询三件套（全部 / 指定 id / 切换策略）的读写都在 proxy-rotate-config
    ...rotateDraftOf(rotate ? config : null),
    protocol: config?.protocol === 'socks5' ? 'socks5' : 'http',
    host: config?.host || '',
    port: config?.port === undefined || config?.port === null ? '' : String(config.port),
    username: config?.username || '',
    password: config?.password || '',
  }
}

/** 草稿 → 接口 payload；非法输入抛出「面向用户」的错误（消息直接进 toast / 弹窗状态行） */
export function readProxyDraft(draft: ProxyDraft): ProxyPayload {
  if (draft.mode === 'none') return null
  if (draft.mode === 'retired') throw new Error(t('原代理来源已下线，请选择已保存的代理、自定义地址或直连'))
  if (draft.mode === 'clash') {
    if (!draft.listenerUid) throw new Error(t('请先选择 Clash Verge 出口'))
    return { source: 'clash', listenerUid: draft.listenerUid }
  }
  if (draft.mode === 'pool') {
    if (!draft.proxyId) throw new Error(t('请先选择已保存的代理'))
    return { source: 'pool', proxyId: draft.proxyId }
  }
  if (draft.mode === 'rotate') {
    // 形状与后端归一逐字段对齐（见 proxy-rotate-config 的模块头）
    const payload = rotatePayloadOf(draft)
    if (!payload) throw new Error(t('轮询至少要选择一条代理（或改选「全部」）'))
    return payload
  }
  const host = draft.host.trim()
  const port = Number(draft.port)
  if (!host) throw new Error(t('请填写代理主机地址'))
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error(t('代理端口必须是 1-65535 的整数'))
  return {
    source: 'custom',
    protocol: draft.protocol === 'socks5' ? 'socks5' : 'http',
    host,
    port,
    username: draft.username.trim(),
    password: draft.password,
  }
}

function clashOptionLabel(option: NonNullable<ClashProxySnapshot['options']>[number]): string {
  return `${option.name || option.uid} :${option.port}${option.profileActive === false ? t('（其他订阅，可能未生效）') : ''}${option.enabled === false ? t('（已在 Clash 中禁用）') : ''}`
}

export function ProxyForm({
  draft,
  onChange,
  idPrefix,
  allowRotate = false,
}: {
  draft: ProxyDraft
  onChange: (next: ProxyDraft) => void
  idPrefix: string
  /** 是否给出「轮询」档（默认不给：调用方要显式开启，见各调用点的说明） */
  allowRotate?: boolean
}) {
  const [testing, setTesting] = React.useState(false)
  const [testResult, setTestResult] = React.useState<React.ReactNode>(null)
  /** 池列表：「已保存的代理」与「轮询」两档共用的选项（模块级缓存，见 accounts-data） */
  const [pool, setPool] = React.useState<PoolItem[] | null>(null)
  const [poolError, setPoolError] = React.useState('')
  const [clash, setClash] = React.useState<ClashProxySnapshot | null>(null)
  const [clashError, setClashError] = React.useState('')
  const options = clash?.options || []
  const set = (patch: Partial<ProxyDraft>): void => onChange({ ...draft, ...patch })

  const loadClash = React.useCallback((): void => {
    setClashError('')
    void Promise.resolve(shared().workbuddyDesktop?.getProxies?.()).then(data => {
      if (!data?.clash || !Array.isArray(data.clash.options)) throw new Error(t('代理列表响应异常'))
      setClash(data.clash)
    }).catch(error => {
      setClash({ available: false, options: [] })
      setClashError(errorMessage(error))
    })
  }, [])
  React.useEffect(() => { if (draft.mode === 'clash' && clash === null) loadClash() }, [draft.mode, clash, loadClash])

  /** 读一次代理池（表单里两个入口：首次挂载与「重新读取」） */
  const loadPool = React.useCallback((force = false): void => {
    setPoolError('')
    void proxyPoolOptions({ force })
      .then(items => setPool(items))
      .catch(error => {
        setPool([])
        setPoolError(errorMessage(error))
      })
  }, [])
  // 只有选中（或初始就是）引用池的那两档时才真正去读 —— 其余档的用户不必为一次
  // 多余的请求买单；读完缓存住，切到这一档不会再打网络
  React.useEffect(() => {
    if ((draft.mode === 'pool' || draft.mode === 'rotate') && pool === null) loadPool()
  }, [draft.mode, pool, loadPool])

  /**
   * 轮询档当前会用到哪些条目（与后端 `proxy_rotate::candidate_ids` 同口径：
   * 「全部」= 池里启用的条目，顺序即池序；指定档 = 表单里选的那些）。
   *
   * 池清单本身来自 `/api/proxies/pool/usable`（后端已滤掉禁用项），这里的
   * `enabled !== false` 只是对旧壳回落路径的兜底 —— 老壳没有那条接口时拿到
   * 的是整份列表，禁用项会出现在里面。
   */
  function rotateCandidateIds(): string[] {
    if (draft.rotateAll) {
      return (pool ?? []).filter(item => item.enabled !== false).map(item => item.id)
    }
    return [...draft.rotateIds]
  }

  /** 用当前表单内容测试出口连通性 */
  async function test(): Promise<void> {
    if (testing) return
    // 轮询档：逐条测候选（每条都走 testProxyPoolItem —— 结果会记进那条代理的
    // 「上次测试」，与代理页那颗按钮同一个动作）。候选可能几十条，全测一遍是
    // 几十次往返，所以只测前 ROTATE_TEST_LIMIT 条并把「还有几条没测」说出来；
    // 只为看某一条通不通时，用「网络代理」页的行内「测试」更直接。
    if (draft.mode === 'rotate') {
      const ids = rotateCandidateIds()
      if (!ids.length) {
        toast(t('轮询至少要选择一条代理（或改选「全部」）'), 'err')
        return
      }
      const targets = ids.slice(0, ROTATE_TEST_LIMIT)
      setTesting(true)
      setTestResult(t('正在连接上游…'))
      let ok = 0
      const failures: string[] = []
      for (const id of targets) {
        const name = pool?.find(item => item.id === id)?.name || id
        try {
          const data = await shared().workbuddyDesktop?.testProxyPoolItem?.(id)
          if (data?.success) ok += 1
          else failures.push(`${name}（${data?.error || t('连接失败')}）`)
        } catch (error) {
          failures.push(`${name}（${errorMessage(error)}）`)
        }
      }
      const rest = ids.length - targets.length
      const summary = t('✅ {ok}/{total} 个出口可用', { ok, total: targets.length })
        + (rest > 0 ? t('（只测了前 {limit} 个，还有 {rest} 个未测）', { limit: ROTATE_TEST_LIMIT, rest }) : '')
      setTestResult(
        <span className={failures.length ? 'text-destructive' : 'text-success'}>
          {summary}{failures.length ? `　${esc(failures.join(t('；')))}` : ''}
        </span>,
      )
      setTesting(false)
      return
    }
    // 池引用：测的是**那条已保存的条目**（testProxyPoolItem 会把结果记进它的
    // 「上次测试」，与代理页那一颗按钮同一个动作）—— 未选条目时给出提示
    if (draft.mode === 'pool') {
      if (!draft.proxyId) {
        toast(t('请先选择已保存的代理'), 'err')
        return
      }
      setTesting(true)
      setTestResult(t('正在连接上游…'))
      try {
        const data = await shared().workbuddyDesktop?.testProxyPoolItem?.(draft.proxyId)
        setTestResult(data?.success
          ? (
              <span className='text-success'>
                {t('✅ 出口可用')}
                {data.ip ? t('　出口 IP {ip}', { ip: esc(data.ip) }) : ''}
                {data.durationMs !== undefined && data.durationMs !== '' ? `　${esc(String(data.durationMs))}ms` : ''}
              </span>
            )
          : <span className='text-destructive'>❌ {esc(data?.error || t('连接失败'))}</span>)
      } catch (error) {
        setTestResult(<span className='text-destructive'>❌ {esc(errorMessage(error))}</span>)
      } finally {
        setTesting(false)
      }
      return
    }
    let proxy: ProxyPayload
    try {
      proxy = readProxyDraft(draft)
    } catch (error) {
      toast(errorMessage(error), 'err')
      return
    }
    setTesting(true)
    setTestResult(t('正在连接上游…'))
    try {
      const data = await shared().workbuddyDesktop?.testProxy?.({ proxy })
      if (data?.success) {
        setTestResult(
          <span className='text-success'>
            {t('✅ 出口可用')}
            {data.status !== undefined && data.status !== '' ? `　HTTP ${esc(String(data.status))}` : ''}
            {data.ip ? t('　出口 IP {ip}', { ip: esc(data.ip) }) : ''}
            {data.durationMs !== undefined && data.durationMs !== '' ? `　${esc(String(data.durationMs))}ms` : ''}
          </span>,
        )
      } else {
        setTestResult(<span className='text-destructive'>❌ {esc(data?.error || t('连接失败'))}</span>)
      }
    } catch (error) {
      setTestResult(<span className='text-destructive'>❌ {esc(errorMessage(error))}</span>)
    } finally {
      setTesting(false)
    }
  }

  return (
    <div className='acct-proxy-form flex flex-col'>
      {draft.mode === 'retired' ? <p className='mb-3 text-xs text-destructive'>{t('原代理来源已下线，请选择已保存的代理、自定义地址或直连')}</p> : null}
      {/* 出网方式：档位一行。走组件库的分段控件（与添加账号弹窗同一档尺寸）——
          这里原先拿 RadioGroup 拼了一套自己的观感，跟别处的分段对不上。 */}
      <SegmentedControl<ProxyDraft['mode']>
        aria-label={t('代理方式')}
        className={DIALOG_SEG_CLASS}
        value={draft.mode}
        onValueChange={value => set({ mode: value })}
        options={[
          { value: 'none', label: t('无代理（直连）') },
          { value: 'pool', label: t('已保存的代理') },
          // 轮询档的可见性由调用方决定（账号设置 / 批量弹窗都为真；
          // 「登录出网代理」那种一次性链路不给）
          ...(allowRotate ? [{
            value: 'rotate' as const,
            label: t('轮询（换 IP）'),
            title: t('逐请求轮换出口（换 IP）：上游按 IP 限流时，换一条出口往往就能过；切到哪条受故障策略与出口冷却约束'),
          }] : []),
          // Clash Verge 档已下线：只有现存配置还停在它上面时才补出这一项，
          // 让用户能看见当前值、并有机会改成别的（补出来就没法切走了）
          ...(draft.mode === 'clash' ? [{ value: 'clash' as const, label: 'Clash Verge' }] : []),
          { value: 'custom', label: t('自定义') },
        ]}
      />

      {draft.mode === 'pool' ? (
        <div className='mt-3'>
          <Field orientation='horizontal' className='field-row'>
            <FieldLabel htmlFor={`${idPrefix}-pool-exit`}>{t('代理')}</FieldLabel>
            {/* value 恒为字符串（空串 = 还没选）：受控值从 undefined 切到字符串会被 Base UI
                当成「非受控 → 受控」的切换，所以不给 undefined */}
            <Select value={draft.proxyId} disabled={!pool?.length}
              onValueChange={value => set({ proxyId: String(value) })}>
              <SelectTrigger id={`${idPrefix}-pool-exit`} className='min-w-[220px]'>
                <SelectValue>
                  {pool?.find(item => item.id === draft.proxyId)
                    ? poolItemLabel(pool.find(item => item.id === draft.proxyId)!)
                    : draft.proxyId ? t('当前代理不可用，请重新选择') : (pool === null ? t('正在读取…') : pool.length ? t('请选择代理') : t('还没有保存的代理'))}
                </SelectValue>
              </SelectTrigger>
              <SelectContent>
                {(pool ?? []).map(item => (
                  // 本页禁用的条目**根本不在列表里**：这份选项来自
                  // `/api/proxies/pool/usable`（后端滤掉禁用项，见
                  // `api::proxies::pool_usable`）—— 禁用的出口不该被选成
                  // 「给这个账号用的那条」。仍可能出现的不可选项只有两类：
                  // Clash Verge 里停用的监听器、以及订阅里已移除的节点 ——
                  // 那两类本页开了也不生效，置灰但列出来并写明原因（凭空消失
                  // 会让用户以为池里没这条）。
                  <SelectItem key={item.id} value={item.id} disabled={!poolItemSelectable(item)}>
                    {poolItemLabel(item)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant='outline' size='sm' onClick={() => loadPool(true)}>{t('重新读取')}</Button>
          </Field>
          <div className='detail mt-1.5'>
            {poolError
              ? t('读取失败：{error}', { error: poolError })
              : pool === null
                ? t('正在读取代理列表…')
                : pool.length
                  ? t('地址与端口由「网络代理」页管理：那边改一次、测一次，所有引用它的账号一起生效；已禁用的节点不出现在这里')
                  : t('「网络代理」页还没有出口 —— 去那里添加手动地址或同步 Clash Verge 的端点')}
          </div>
        </div>
      ) : null}

      {draft.mode === 'rotate' ? (
        <div className='mt-3'>
          {/* 切换策略排在最前：它决定「什么失败才算该换」，读这一档的人
              第一个问题就是这个。默认 smart —— 业务错误换出口照样失败，
              给它记冷却只会把好出口白白冻住。 */}
          <Field orientation='horizontal' className='field-row mb-2.5'>
            <FieldLabel htmlFor={`${idPrefix}-failure-mode`}>{t('切换策略')}</FieldLabel>
            <Select value={draft.failureMode}
              onValueChange={value => set({ failureMode: value === 'all_errors' ? 'all_errors' : 'smart' })}>
              <SelectTrigger id={`${idPrefix}-failure-mode`} className='min-w-[220px]'
                aria-describedby={`${idPrefix}-failure-mode-help`}>
                <SelectValue>
                  {draft.failureMode === 'all_errors' ? t('所有上游错误都切换') : t('智能切换（推荐）')}
                </SelectValue>
              </SelectTrigger>
              <SelectContent>
                <SelectItem value='smart'>{t('智能切换（推荐）')}</SelectItem>
                <SelectItem value='all_errors'>{t('所有上游错误都切换')}</SelectItem>
              </SelectContent>
            </Select>
          </Field>
          <p id={`${idPrefix}-failure-mode-help`} className='detail mb-2.5'>
            {draft.failureMode === 'all_errors'
              ? t('激进模式：任何 HTTP 4xx／5xx 上游错误都算一次切换候选，仍受尝试预算与出口冷却约束，不保证遍历全部节点；已经成功响应过的流不会被重放。')
              : t('智能模式：确定性的参数或鉴权错误不遍历节点；网络失败与 HTTP 500 有限尝试，HTTP 502–504 可切换，HTTP 403 切换节点；普通 HTTP 429（含 rate_limit）换出口，明确余额或日／月配额耗尽的 429 仍按账号处理；Retry-After 冷却原出口，可立即尝试其他出口；仍受尝试预算与出口冷却约束。')}
          </p>
          {/* 「全部」是默认档：勾上 = 池里每个启用的出口轮流使用（新加的出口
              自动进轮询，不必回来改这条账号）。取消勾选才出现多选。
              这一行刻意**不走 .field-row**：账号设置弹窗里它的直接 label 子元素
              会被 `.acct-dlg-main .field-row > label` 强制成 76px 定宽字段标签，
              「全部代理节点（推荐）」比 76px 长，nowrap 溢出会压住后面的说明
              （实测叠字）—— 所以用普通 flex 行，文案跟在复选框后面自然排 */}
          <div className='flex flex-wrap items-center gap-x-2.5 gap-y-1'>
            <Checkbox id={`${idPrefix}-rotate-all`} checked={draft.rotateAll}
              aria-label={t('轮询全部代理节点')}
              onCheckedChange={value => set({ rotateAll: value === true })} />
            <Label htmlFor={`${idPrefix}-rotate-all`} className='cursor-pointer text-xs text-subtle'>
              {t('全部代理节点（推荐）')}
            </Label>
            <span className='detail'>
              {t('「网络代理」页里每个启用的出口轮流使用；之后新加的出口自动加入')}
            </span>
          </div>
          {/* 与后端 `proxy_rotate::candidate_ids` 同口径的实时读数：改完范围
              马上能看到「实际会参与的有几条」，不必切出去看代理页 */}
          <p className='detail mt-1.5'>
            {pool === null
              ? t('正在读取代理列表…')
              : t('已启用候选 {n} 个', { n: rotateCandidateIds().length })}
          </p>
          {!draft.rotateAll ? (
            <Field orientation='horizontal' className='field-row mt-2.5'>
              <FieldLabel htmlFor={`${idPrefix}-rotate-exits`}>{t('轮询的代理')}</FieldLabel>
              <MultiSelect id={`${idPrefix}-rotate-exits`}
                value={draft.rotateIds}
                onValueChange={values => set({ rotateIds: values.map(String) })}
                // 候选来自 `/api/proxies/pool/usable`：禁用项已被后端滤掉，
                // 不会出现在轮询候选里。仍可能出现的不可选项只有「Clash 侧停用」
                // 与「订阅里已移除」两类 —— 那两类本页开了也不生效，置灰但列出。
                options={(pool ?? []).map(item => ({
                  value: item.id,
                  label: poolItemLabel(item),
                  disabled: !poolItemSelectable(item),
                }))}
                placeholder={t('选择要轮询的代理（可多选）')}
                aria-label={t('轮询的代理')} />
              <Button variant='outline' size='sm' onClick={() => loadPool(true)}>{t('重新读取')}</Button>
            </Field>
          ) : null}
          <div className='detail mt-1.5'>
            {poolError
              ? t('读取失败：{error}', { error: poolError })
              : pool === null
                ? t('正在读取代理列表…')
                : pool.length
                  ? t('每条请求换一个出口（按顺序轮流），上游看到的 IP 因此是变化的；出口的地址与端口由「网络代理」页管理')
                  : t('「网络代理」页还没有出口 —— 去那里添加手动地址或同步 Clash Verge 的端点')}
          </div>
        </div>
      ) : null}

      {draft.mode === 'clash' ? <div className='mt-3'>
        <Field orientation='horizontal' className='field-row'>
          <FieldLabel htmlFor={`${idPrefix}-clash-exit`}>{t('出口')}</FieldLabel>
          <Select value={draft.listenerUid} disabled={!options.length} onValueChange={value => set({ listenerUid: String(value) })}>
            <SelectTrigger id={`${idPrefix}-clash-exit`} className='min-w-[220px]'>
              <SelectValue>{options.find(option => option.uid === draft.listenerUid)
                ? clashOptionLabel(options.find(option => option.uid === draft.listenerUid)!)
                : draft.listenerUid ? t('当前代理不可用，请重新选择') : t('请选择出口')}</SelectValue>
            </SelectTrigger>
            <SelectContent>{options.filter(option => option.uid).map(option => <SelectItem key={option.uid} value={String(option.uid)} disabled={option.enabled === false}>
              {clashOptionLabel(option)}
            </SelectItem>)}</SelectContent>
          </Select>
          <Button variant='outline' size='sm' onClick={loadClash}>{t('重新读取')}</Button>
        </Field>
        <p className='detail mt-1.5'>{clashError ? t('读取失败：{error}', { error: clashError })
          : clash === null ? t('正在读取 Clash Verge 配置…')
            : clash.available === false ? t('不可用：{error}', { error: clash.error || t('未检测到 Clash Verge 配置') })
              : t('读取自 {dir}；端口由 Clash Verge 管理，这里实时同步', { dir: clash.dir || 'Clash Verge' })}</p>
        <p className='detail mt-1.5'>{t('既有 Clash Verge 直接引用继续可用；也可在「网络代理」页同步端点，再改选「已保存的代理」。')}</p>
      </div> : null}

      {draft.mode === 'custom' ? (
        <div className='mt-3'>
          <FieldGroup orientation='horizontal' className='field-row'>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-protocol`}>{t('协议')}</FieldLabel>
              <Select value={draft.protocol} onValueChange={value => set({ protocol: value as 'http' | 'socks5' })}>
                <SelectTrigger id={`${idPrefix}-protocol`} className='min-w-[110px]'>
                  <SelectValue>{draft.protocol === 'socks5' ? 'SOCKS5' : 'HTTP'}</SelectValue>
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value='http'>HTTP</SelectItem>
                  <SelectItem value='socks5'>SOCKS5</SelectItem>
                </SelectContent>
              </Select>
            </Field>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-host`} className='ml-2'>{t('主机')}</FieldLabel>
              <Input id={`${idPrefix}-host`} className='min-w-[140px]' placeholder='127.0.0.1'
                value={draft.host} onChange={event => set({ host: event.currentTarget.value })} />
            </Field>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-port`}>{t('端口')}</FieldLabel>
              <Input id={`${idPrefix}-port`} type='number' min={1} max={65535} className='max-w-[110px]'
                placeholder='7890' value={draft.port} onChange={event => set({ port: event.currentTarget.value })} />
            </Field>
          </FieldGroup>
          <FieldGroup orientation='horizontal' className='field-row mt-2.5'>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-user`}>{t('用户名')}</FieldLabel>
              <Input id={`${idPrefix}-user`} placeholder={t('可选')} autoComplete='off'
                value={draft.username} onChange={event => set({ username: event.currentTarget.value })} />
            </Field>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-pass`}>{t('密码')}</FieldLabel>
              <Input id={`${idPrefix}-pass`} type='password' placeholder={t('可选')} autoComplete='new-password'
                value={draft.password} onChange={event => set({ password: event.currentTarget.value })} />
            </Field>
          </FieldGroup>
        </div>
      ) : null}

      <FieldGroup orientation='horizontal' className='field-row mt-3'>
        <Button variant='outline' size='sm' disabled={testing} onClick={() => void test()}>
          {testing ? t('测试中…') : t('测试出口')}
        </Button>
        <span className='detail'>{testResult}</span>
      </FieldGroup>
    </div>
  )
}

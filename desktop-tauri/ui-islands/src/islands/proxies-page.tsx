/**
 * 网络代理页：代理池（列表 / 新增 / 编辑 / 删除 / 启停 / 连通性测试 / 勾选
 * 批量测试与批量删除）+ 订阅区块 + 内核状态条。
 *
 * 存储与解析在 Rust 侧：`core::proxy_pool` 是池的唯一事实来源（`kv` 的
 * `proxyPool` 键），`core::subscriptions` + `core::sub_kernel` 是订阅与内核的
 * 事实来源。HTTP 契约见 `api::proxies` / `api::subscriptions` 的模块头。
 *
 * ── 三种条目（只读语义不同）──────────────────────────────────
 *   · 手动：这里新建 / 编辑 / 删除，字段全在本地；
 *   · Clash Verge 同步来的：**只读镜像** —— 名称与端口跟随 Clash，本页不给
 *     编辑与删除入口，要改去 Clash Verge 改（下次同步自然摘掉）；
 *   · 订阅节点：**只读镜像** —— 由内核起成本地监听器后镜像进来；名称与端口
 *     跟随订阅与内核。两条例外可以删：所属订阅已删（孤儿）与订阅里已移除
 *     这个节点（gone）—— 它们不会再被任何同步救回来，删除是唯一出路。
 *   「启用」列三种来源都放行：它管的是**本端用不用它**（禁用的条目不再被
 *   账号下拉列出、也不再进轮询候选）。
 *
 * ── 按「提供方」筛选（下拉）────────────────────────────────────
 * 来源分段控件只到「手动 / Clash Verge / 订阅」这一层，而订阅那档底下常有
 * 好几家机场（每家几十上百条）——「只想看某家机场的节点」在那层筛不出来。
 * 工具条里因此多一个**提供方下拉**（排在来源分段控件之前）：手动 / Clash /
 * 全部订阅 / 各家订阅（名字与条数来自订阅清单本身，不靠节点名猜）/
 * 已移除的订阅（孤儿节点）。两个控件**联动**：选中某一家时来源自动落到
 * 「订阅」，点来源分段时提供方回到「全部提供方」—— 否则会出现「手动 + 某家
 * 机场」这种永远筛不出东西、也看不出是谁在挡着的组合。
 *
 * ── 订阅区块与内核状态条──────────────���─────────────────────────
 * 订阅区块（`SubscriptionSection`）放在池表格之后：它是「出口从哪来」的上游
 * 环节，顺序上先看池（当前可用出口）再看订阅（供给来源）更符合排查习惯。
 * 内核状态条提到**页面级**（`KernelBar`）—— 它讲的是「本机内核 / 节点端口」
 * 这件全局的事，端口用尽的报错也属于它。
 *
 * ── 清除轮询候选冷却 ──────────────────────────────────────────
 * 轮询档会把刚失败过的出口冷却一段时间（免得「换一条」变成「又换回刚坏的
 * 那条」）。这条管理动作只清那张表：不动账号限额、不重启内核、不受当前筛选
 * 或勾选影响（后端只接受空 body）。桥方法缺失时按钮不出现 —— 摆一颗点了没
 * 反应的按钮比没有更糟。
 */

import * as React from 'react'
import { createRoot } from 'react-dom/client'
import {
  Badge, Button, Card, CardHeader, Checkbox, InputGroup, InputGroupAddon,
  InputGroupInput, ResponsiveSheet, SegmentedControl, Select, SelectContent,
  SelectItem, SelectTrigger, SelectValue, Switch, Table, TableBody, TableCell,
  TableHead, TableHeader, TableRow, cn,
} from '@ui'
import { poolItemAddress } from './accounts-shared'
import {
  batchRemove, batchTest, clearRotateCooldowns, clearSelection, durationText, formatTime,
  getSnapshot, isPicked, isReadonlyPoolItem, items, loadPanel, removeItem, setAllPicked,
  subscribe, syncClash, testItem, toggleItem, togglePick, wb, type ProxyPoolItem,
} from './proxies-state'
import {
  ORPHAN_VENDOR, groupOf, isGoneSubscriptionItem, isOrphanSubscriptionItem,
  isRemovedSubscriptionItem, vendorHits,
} from './proxy-pool-classify'
import { ProxyModal, ProxySettingsModal } from './proxies-dialogs'
import { KernelBar, KernelDialog, SubscriptionManagerDialog } from './proxies-sub-view'
import {
  getSnapshot as getSubSnapshot, loadSubscriptions,
  subscribe as subscribeSubs, type SubscriptionItem,
} from './proxies-sub-state'
import { COL_CLASS, closeColumnPanel, repaintColumnWidths, setupColumns, visibleColumns } from './proxies-columns'
import { TableFooter, useClientPaging } from './table-shell'
import { t } from '../i18n'
import { proxyServiceLabels } from '../../../ui-shared/proxy-usage'

/// 进入「网络代理」页时的一次性动作。
///
/// 现在没有要做的事，但函数本身留着：`wbProxiesPanel.activate` 是对外契约
/// （切回本页会调它），删掉会让调用方拿到 undefined。
function enteredPage(): void {}

function ProxiesPage() {
  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot)
  // 订阅清单：条数给「提供方」下拉认家（菜单里的档位不靠节点名猜，
  // 就是这份清单 + 池条目上的 providerId）
  const subSnapshot = React.useSyncExternalStore(subscribeSubs, getSubSnapshot)
  const subs = subSnapshot.items
  // 内核状态读快照里的那一份（`proxies-sub-state` 只导出快照读取函数，
  // 没有再单独导一个 kernel 选择器 —— 页面直接用同一个快照）
  const subKernel = subSnapshot.kernel
  const subBusy = subSnapshot.busy
  const [modal, setModal] = React.useState<{ item: ProxyPoolItem | null } | null>(null)
  const [settingsOpen, setSettingsOpen] = React.useState(false)
  const [kernelOpen, setKernelOpen] = React.useState(false)
  const [subsOpen, setSubsOpen] = React.useState(false)
  const [filterOpen, setFilterOpen] = React.useState(false)
  const [keyword, setKeyword] = React.useState('')
  const [vendor, setVendor] = React.useState('all')
  const [source, setSource] = React.useState('all')
  const [status, setStatus] = React.useState('all')
  const [colRetry, setColRetry] = React.useState(0)
  const [, forceTick] = React.useReducer((tick: number) => tick + 1, 0)
  const rows = items()
  const ready = snapshot.data !== null
  const selectedCount = snapshot.selected.size
  const progress = snapshot.batchProgress
  const locked = snapshot.syncing || snapshot.batchBusy || snapshot.testing !== null || snapshot.pending.size > 0
  const columns = visibleColumns()
  const query = keyword.trim().toLowerCase()
  const clashPicked = rows.filter(item => snapshot.selected.has(item.id) && isReadonlyPoolItem(item)).length
  const vendors = vendorOptions(rows, subs)

  const shown = rows.filter(item => (source === 'all' || groupOf(item) === source)
    && vendorHits(item, vendor)
    && (status === 'all' || stateOf(item) === status)
    && (!query || [item.name, poolItemAddress(item), item.host, item.protocol, String(item.port ?? '')].join(' ').toLowerCase().includes(query)))
  const shownIds = shown.map(item => item.id)
  const allPicked = shownIds.length > 0 && shownIds.every(id => snapshot.selected.has(id))
  const somePicked = shownIds.some(id => snapshot.selected.has(id)) && !allPicked
  const paging = useClientPaging(shown.length, 'proxies')
  const pageRows = paging.paged ? paging.slice(shown) : shown
  /** 各组在**整个命中集**里的条数：分组带报的是这一组有多少条，不是这一页有多少条 */
  const groupCounts = new Map<string, number>()
  for (const item of shown) groupCounts.set(groupOf(item), (groupCounts.get(groupOf(item)) ?? 0) + 1)

  // 订阅清单进页面就拉一次：提供方下拉的档位来自它（0 条的档也要能选）
  React.useEffect(() => { void loadSubscriptions() }, [])
  React.useEffect(() => { paging.goto(1) }, [query, source, vendor, status])
  React.useLayoutEffect(() => {
    if (!setupColumns(() => forceTick())) {
      const timer = window.setTimeout(() => setColRetry(tick => tick + 1), 50)
      return () => window.clearTimeout(timer)
    }
    forceTick()
    return undefined
  }, [colRetry])
  React.useLayoutEffect(() => { repaintColumnWidths() })
  React.useEffect(() => () => { closeColumnPanel() }, [])

  const filtersActive = Boolean(keyword) || vendor !== 'all' || source !== 'all' || status !== 'all'

  function clearFilters(): void { setKeyword(''); setVendor('all'); setSource('all'); setStatus('all') }

  /**
   * 选提供方：来源那一档跟着走（选具体某家 = 来源落到「订阅」；选手动 /
   * Clash = 来源落到同名那档）。两个控件各筛各的会筛出「手动 + 某家机场」
   * 这种恒空的组合，而用户看不出是谁在挡着 —— 联动之后任一个控件都随时给出
   * 一个有效视图。
   */
  function pickVendor(next: string | null): void {
    const value = next ?? 'all'
    setVendor(value)
    if (value === 'all') setSource('all')
    else if (value === 'manual' || value === 'clash') setSource(value)
    else setSource('subscription') // 全部订阅 / 某一家订阅 / 已移除的订阅
  }

  /** 选来源粗档：提供方细筛回到「全部提供方」（同上，避免恒空的组合） */
  function pickSource(next: string): void {
    setSource(next)
    setVendor('all')
  }

  function cell(column: string, item: ProxyPoolItem): React.ReactNode {
    switch (column) {
      case 'pick':
        return <Checkbox checked={isPicked(item.id)} disabled={locked}
          aria-label={t('勾选「{name}」', { name: item.name || t('未命名') })}
          onCheckedChange={next => togglePick(item.id, next)} />
      case 'name':
        return <div className='flex min-w-0 flex-col gap-1'><b>{item.name || t('未命名')}</b>
          <span className='flex items-center gap-2 text-[10px] text-subtle'>
            <Badge variant='outline' shape='tag' className='font-mono text-[10px]'>{(item.resolved?.protocol || item.protocol || 'http').toUpperCase()}</Badge>
            {item.source === 'clash' ? <Badge variant='secondary' shape='tag' className='text-[10px]'>Clash Verge</Badge> : null}
            {/* 订阅节点：标出来源，并显示「端口未监听」——
                这类条目最容易被误认为可用（端口字段有值、但那会儿没人听）。
                判据是 `resolved`：后端解析时已经探活过（见 proxy_pool::resolve_item）。 */}
            {item.source === 'subscription' ? (
              <Badge variant='secondary' shape='tag' className='text-[10px]'>{t('订阅节点')}</Badge>
            ) : null}
            {item.source === 'subscription' && !item.resolved ? (
              <span className='text-[10px] text-warning' title={item.resolveError || ''}>
                {t('端口未监听')}
              </span>
            ) : null}
            {isGoneSubscriptionItem(item) ? (
              <span className='text-[10px] text-warning'>{t('订阅里已移除')}</span>
            ) : isOrphanSubscriptionItem(item) ? (
              <span className='text-[10px] text-warning'>{t('所属订阅已删除')}</span>
            ) : null}
            {item.enabled === false ? t('已禁用') : null}
          </span></div>
      case 'addr': {
        // 订阅里已没有这个节点：地址列要说明「不是坏了、是被订阅移除了」，
        // 并点出出路（这一行可以直接删除）
        if (isGoneSubscriptionItem(item)) {
          return <span className='text-subtle'
            title={t('订阅里已经没有这个节点了（更新订阅后它不在清单里）。这一行可以删除 —— 删除按钮只清掉代理池里的这条记录。')}>
            {t('已从订阅移除')}
          </span>
        }
        const address = poolItemAddress(item)
        return <div className='flex min-w-0 items-center gap-1'>
          <span className='min-w-0 flex-1 break-all font-mono text-xs leading-[1.5] text-text-2'
            title={item.resolveError || address}>{address || t('地址不完整')}</span>
          {address ? <Button variant='ghost' size='icon-xs' className='copy-btn shrink-0' data-copy={address} title={t('复制地址')}>⧉</Button> : null}
        </div>
      }
      case 'usage': {
        const services = proxyServiceLabels(item, t)
        return <div className='flex flex-col gap-0.5 text-xs text-subtle'>
          {item.usedBy?.length ? <span title={item.usedBy.map(entry => entry.name || entry.id).join(t('、'))}>
            {t('{n} 个账号在用', { n: item.usedBy.length })}</span> : null}
          {services.map(service => <span key={service}>{service}</span>)}
          {!item.usedBy?.length && !services.length ? t('暂未使用') : null}
        </div>
      }
      case 'state':
        return <Switch checked={item.enabled !== false} disabled={locked || item.source === 'clash' && item.sourceEnabled === false}
          title={item.source === 'clash'
            ? item.sourceEnabled === false ? t('这条在 Clash Verge 里是停用的，本页启用不生效 —— 到 Clash Verge 里打开它')
              : t('禁用后本端不再用它（引用它的账号回退直连）；端口在 Clash Verge 侧，不受影响')
            : t('禁用后本端不再用它：账号下拉与轮询候选都不再列出它')}
          aria-label={t('启用「{name}」', { name: item.name || t('未命名') })}
          onCheckedChange={next => void toggleItem(item, next)} />
      case 'test': {
        if (snapshot.testing === item.id) return <span className='text-subtle'>{t('测试中…')}</span>
        const last = item.lastTest
        if (!last?.at) return <span className='text-subtle'>{t('未检测')}</span>
        return <div className='flex min-w-0 flex-col gap-0.5 text-xs leading-[1.5]'>
          <span className={last.success ? 'text-success' : 'text-destructive'}>
            {last.success ? t('✅ 连接成功') : t('❌ 连接失败')}　{durationText(last.durationMs)}
          </span>
          <span className='truncate text-[10px] text-subtle' title={last.error || undefined}>
            {last.success ? last.ip || '' : last.error || t('连接失败')}
          </span>
          <span className='text-[10px] text-subtle'>{formatTime(last.at)}</span>
        </div>
      }
      case 'act': {
        // 手动条目给编辑 / 删除；只读条目只留测试。两条例外放行删除：
        // 孤儿订阅节点（所属订阅已删）与订阅里已移除的节点 —— 归属的订阅
        // 不认识它们了，删除是清理它们的唯一出路。
        const removable = !isReadonlyPoolItem(item) || isOrphanSubscriptionItem(item) || isGoneSubscriptionItem(item)
        return <div className='row-actions'>
          <Button size='sm' variant='ghost' disabled={locked} onClick={() => void testItem(item)}>{t('测试')}</Button>
          {!isReadonlyPoolItem(item) ? (
            <Button size='sm' variant='ghost' disabled={locked} onClick={() => setModal({ item })}>{t('编辑')}</Button>
          ) : (
            <span className='text-[10px] text-subtle'
              title={item.source === 'clash'
                ? t('名字与端口在 Clash Verge 里改；启用可在本页关闭')
                : t('名字与端口跟随订阅与内核；启用可在本页关闭')}>
              {item.source === 'clash' ? t('只读：在 Clash Verge 里改') : t('只读：在「订阅」里改')}
            </span>
          )}
          {removable ? (
            <Button size='sm' variant='ghost' className={isReadonlyPoolItem(item) ? 'text-destructive' : 'text-destructive'}
              disabled={locked} onClick={() => void removeItem(item)}>{t('删除')}</Button>
          ) : null}
        </div>
      }
      default: return null
    }
  }

  /**
   * 表体：**本页**的行 + 它们各自的分组带。
   *
   * 分组带夹在行的中间（组一变就插一条），所以带子是「当前这一页里出现过的组」
   * —— 订阅那组跨页时，第 2 页开头仍会看到它的带与只读口径。带上的条数是该组
   * 在整个命中集里的总数（不是这一页的条数）。
   */
  function body(): React.ReactNode {
    if (!rows.length) {
      return <TableRow><TableCell colSpan={columns.length} className='empty'>
        {!ready ? snapshot.error || t('加载中…') : <div className='flex flex-col items-center gap-3 py-8'>
          <b className='text-sm text-foreground'>{t('还没有保存的代理')}</b>
          <p className='text-xs text-subtle'>{t('添加手动 HTTP / SOCKS5 地址，或同步 Clash Verge 的端点，供账号与服务线路使用。')}</p>
          <Button onClick={() => setModal({ item: null })}>{t('添加第一个代理')}</Button>
        </div>}
      </TableCell></TableRow>
    }
    if (!shown.length) {
      return <TableRow><TableCell colSpan={columns.length} className='empty'>
        <div className='flex flex-col items-center gap-3 py-5'>
          <span>{query ? t('没有匹配「{kw}」的出口', { kw: keyword.trim() }) : t('没有匹配当前筛选的出口')}</span>
          {filtersActive ? <Button variant='outline' size='sm' onClick={clearFilters}>{t('清空筛选')}</Button> : null}
        </div>
      </TableCell></TableRow>
    }
    const blocks: React.ReactNode[] = []
    let current = ''
    for (const item of pageRows) {
      const key = groupOf(item)
      if (key !== current) {
        current = key
        const group = groups().find(entry => entry.key === key)
        blocks.push(<TableRow className='tr-group' key={`group:${key}:${blocks.length}`}>
          <TableCell colSpan={columns.length}>
            <span className='prov-tag'>{group?.label ?? key}</span>
            {t('{n} 个', { n: groupCounts.get(key) ?? 0 })}
            {group?.note ? <span className='font-normal'>　·　{group.note}</span> : null}
          </TableCell>
        </TableRow>)
      }
      blocks.push(<TableRow key={item.id} data-id={item.id}
        className={cn(item.enabled === false && 'off', isPicked(item.id) && 'bg-primary-soft')}>
        {columns.map(column => <TableCell key={column.key} className={`ta-${column.align}`}>{cell(column.key, item)}</TableCell>)}
      </TableRow>)
    }
    return <>{blocks}</>
  }

  return <>
    <Card render={<section />} className='panel'>
      <CardHeader className='panel-head'>
        <h2>{t('代理池')}</h2><Badge variant='secondary' shape='tag'>{rows.length}</Badge>
        <span className='panel-sub'>
          {ready ? t('已启用 {n} 个', { n: rows.filter(item => item.enabled !== false).length }) : t('加载中…')}
        </span>
        <span className='text-[11px] text-subtle'>{t('订阅与 Clash 的出口自动镜像进来')}</span>
        <div className='head-actions'>
          <Button variant='outline' disabled={snapshot.batchBusy} onClick={() => setSettingsOpen(true)}>{t('设置')}</Button>
          {/* 订阅管理：订阅表的唯一入口（按 Agent2API 的布局，页面上不再有
              第二个面板）。带订阅条数 —— 0 条时只写按钮名，免得出现
              「订阅管理（0）」这种读数 */}
          <Button variant='outline' disabled={snapshot.batchBusy} onClick={() => setSubsOpen(true)}>
            {subs.length ? t('订阅管理（{n}）', { n: subs.length }) : t('订阅管理')}
          </Button>
          <Button variant='outline' disabled={locked} onClick={() => void syncClash()}>{snapshot.syncing ? t('同步中…') : t('同步 Clash Verge')}</Button>
          <Button variant='outline' disabled={snapshot.loading || locked} onClick={() => void loadPanel()}>{snapshot.loading ? t('刷新中…') : t('刷新')}</Button>
          {/* 桥方法缺失时按钮不出现：摆一颗点了没反应的按钮比没有更糟 */}
          {typeof wb().workbuddyDesktop?.clearProxyRotateCooldowns === 'function' ? (
            <Button variant='outline' disabled={snapshot.clearingCooldowns}
              onClick={() => void clearRotateCooldowns()}>
              {snapshot.clearingCooldowns ? t('清除中…') : t('清除轮询候选冷却')}
            </Button>
          ) : null}
          <Button disabled={locked} onClick={() => setModal({ item: null })}>{t('＋ 新增代理')}</Button>
          <Button variant='ghost' className='hidden max-[767px]:inline-flex' onClick={() => setFilterOpen(true)}>{t('筛选')}</Button>
        </div>
      </CardHeader>

      {/* 内核状态条：本机内核与节点端口是**全局**的事（端口用尽也报在这里），
          不跟着订阅表一起收进区块。「内核设置」打开内核弹窗 —— 内核的完整
          生命周期（下载 / 启动 / 停止 / 删除）都在那里 */}
      <div className='px-4 pb-3'>
        <KernelBar kernel={subKernel} onOpen={() => setKernelOpen(true)} />
      </div>

      {selectedCount ? <div className='batch-bar active'>
        <span className='batch-count'>
          {t('已选')} <b>{selectedCount}</b> {t('个 · 共')} <b>{shown.length}</b> {t('个')}
        </span>
        {clashPicked ? <span className='batch-hidden'>
          {t('其中 {n} 条是同步来的只读条目（Clash / 订阅）：可参与批量测试，删除会跳过', { n: clashPicked })}
        </span> : null}
        <div className='batch-actions'>
          <Button variant='outline' disabled={locked} onClick={() => void batchTest()}>{progress ? t('测试中 {done}/{total}', { done: progress.done, total: progress.total }) : t('批量测试')}</Button>
          <Button variant='destructive' disabled={locked} onClick={() => void batchRemove()}>{t('批量删除')}</Button>
          <Button variant='outline' disabled={locked} onClick={clearSelection}>{t('取消选择')}</Button>
        </div>
      </div> : <ResponsiveSheet open={filterOpen} onOpenChange={setFilterOpen} title={t('筛选')}
        inlineClassName='flex flex-wrap items-center gap-2.5 border-b border-border px-4 py-3'
        bodyClassName='proxy-filter-body'
        actions={<Button variant='ghost' size='sm' disabled={!filtersActive} onClick={clearFilters}>{t('清空')}</Button>}>
        <InputGroup className='w-[clamp(150px,22vw,240px)]'>
          <InputGroupInput type='search' value={keyword} aria-label={t('搜索代理')} placeholder={t('搜索名称 / 地址 / 端口')}
            onChange={event => setKeyword(event.currentTarget.value)} />
          <InputGroupAddon align='inline-start' aria-hidden='true'><SearchIcon /></InputGroupAddon>
        </InputGroup>
        {/* 提供方下拉排在来源分段之前：它比来源更「具体」（来源只到手动 / Clash /
            订阅这一层，而订阅底下是各家机场）。SelectValue 显式给当前档的文案 ——
            组件库的 Select 不靠 value 反查展示文本 */}
        <Select value={vendor} onValueChange={pickVendor}>
          <SelectTrigger className='w-[clamp(140px,18vw,200px)]' aria-label={t('按提供方筛选')}>
            <SelectValue>{vendors.find(option => option.value === vendor)?.label ?? t('全部提供方')}</SelectValue>
          </SelectTrigger>
          <SelectContent>
            {vendors.map(option => (
              <SelectItem key={option.value} value={option.value}>
                {/* 0 条的档弱化显示但**留着**：订阅还在、节点还没落地时，
                    档位凭空消失会让人以为是没订阅上 */}
                <span className='flex w-full items-center gap-2'>
                  <span className={'min-w-0 flex-1 truncate' + (option.count ? '' : ' text-subtle')}>{option.label}</span>
                  <span className='flex-none text-[11px] text-subtle'>{option.count}</span>
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select value={source} onValueChange={value => pickSource(value || 'all')}>
          <SelectTrigger className='w-[120px]' aria-label={t('按来源筛选')}>
            <SelectValue>{sourceOptions(rows).find(option => option.value === source)?.label ?? t('全部')}</SelectValue>
          </SelectTrigger>
          <SelectContent>{sourceOptions(rows).map(option => <SelectItem key={option.value} value={option.value}>
            {option.label}　{option.count}
          </SelectItem>)}</SelectContent>
        </Select>
        <SegmentedControl className='ml-auto' options={statusOptions(rows)} value={status} onValueChange={setStatus} aria-label={t('按状态筛选')} />
      </ResponsiveSheet>}
      <div className='models-table-wrap'>
        <Table container={false} className='models-table proxies-table'>
          <colgroup>{columns.map(column => <col key={column.key} data-col={column.key} className={COL_CLASS[column.key] || undefined} />)}</colgroup>
          <TableHeader><TableRow>{columns.map(column => <TableHead key={column.key} data-col={column.key} className={`ta-${column.align}`}>
            {column.key === 'pick' ? <Checkbox checked={allPicked} indeterminate={somePicked} disabled={!shownIds.length || locked}
              aria-label={t('全选当前筛选出的出口')} onCheckedChange={next => setAllPicked(shownIds, next)} /> : column.label}
          </TableHead>)}</TableRow></TableHeader>
          <TableBody>{body()}</TableBody>
        </Table>
      </div>
      <TableFooter leading={snapshot.error ? <span className='text-destructive'>{t('读取失败：{reason}', { reason: snapshot.error })}</span> : undefined}
        total={shown.length} page={paging.page} pageCount={paging.pageCount} size={paging.size}
        onSizeChange={paging.setSize} onPageChange={paging.goto}
        range={paging.paged ? { start: paging.rangeStart, end: paging.rangeEnd } : null} />
    </Card>
    {/* 订阅表收进「订阅管理」弹窗：它是「配完就不常动」的低频内容，
        而下面的代理池天天要看 —— 两张表叠在一页会把真正高频的那张推到首屏外 */}
    {subsOpen ? <SubscriptionManagerDialog onClose={() => setSubsOpen(false)} /> : null}
    {modal ? <ProxyModal item={modal.item} onClose={() => setModal(null)} /> : null}
    {settingsOpen ? <ProxySettingsModal onClose={() => setSettingsOpen(false)} /> : null}
    {kernelOpen ? <KernelDialog kernel={subKernel} busy={subBusy} onClose={() => setKernelOpen(false)} /> : null}
  </>
}

function stateOf(item: ProxyPoolItem): string {
  if (item.enabled === false) return 'off'
  if (isGoneSubscriptionItem(item) || isOrphanSubscriptionItem(item)) return 'orphan'
  if (!item.lastTest?.at) return 'none'
  return item.lastTest.success ? 'ok' : 'fail'
}

function statusOptions(rows: ProxyPoolItem[]): Array<{ value: string; label: string; count: number }> {
  return [
    { value: 'all', label: t('全部'), count: rows.length },
    { value: 'ok', label: t('可用'), count: rows.filter(item => stateOf(item) === 'ok').length },
    { value: 'fail', label: t('失败'), count: rows.filter(item => stateOf(item) === 'fail').length },
    { value: 'none', label: t('未检测'), count: rows.filter(item => stateOf(item) === 'none').length },
    { value: 'off', label: t('已禁用'), count: rows.filter(item => stateOf(item) === 'off').length },
  ]
}

/* ─── 来源分组 ───────────────────────────────── */

/**
 * 三个来源组（顺序固定：可写的在最上，镜像来的按「配置成本」排在后面）。
 * note 就是这一组的读写口径 —— 分组带是用户唯一会逐组读一行文字的地方。
 */
function groups(): Array<{ key: string; label: string; note: string }> {
  return [
    { key: 'manual', label: t('手动'), note: t('本页可编辑、可删除') },
    { key: 'clash', label: 'Clash Verge', note: t('名字与端口在 Clash Verge 里改；启用可在本页关闭') },
    { key: 'subscription', label: t('订阅'), note: t('名字与端口跟随订阅与内核；启用可在本页关闭') },
  ]
}

/** 来源档位带上各来源的条数（0 条的档仍然保留，只是弱化显示） */
function sourceOptions(rows: ProxyPoolItem[]): Array<{ value: string; label: string; count: number }> {
  const count = (key: string) => rows.filter(item => groupOf(item) === key).length
  return [
    { value: 'all', label: t('全部'), count: rows.length },
    { value: 'manual', label: t('手动'), count: count('manual') },
    { value: 'clash', label: 'Clash', count: count('clash') },
    { value: 'subscription', label: t('订阅'), count: count('subscription') },
  ]
}

/* ─── 提供方档位 ─────────────────────────────── */

/**
 * 提供方下拉的档位：全部 / 手动 / Clash Verge / 全部订阅 / 各家订阅 / 已移除的订阅。
 *
 * ── 家名从哪儿来（不猜）────────────────────────────────────────
 * 名字与 id 都取**订阅清单**（`GET /api/subscriptions` 的 items），池条目只提供
 * `providerId` —— 节点名里带不带机场名是各家自己的命名习惯，拿它反推归属
 * 一改命名就错。清单里有的订阅一律列出（哪怕这一家在池里 0 条：先有订阅、
 * 内核还没起监听器时就是这样）。
 *
 * 各家按池里条数从多到少排：点这个下拉的人多半想快速切到「那一大坨」。
 */
function vendorOptions(
  rows: ProxyPoolItem[],
  subs: SubscriptionItem[],
): Array<{ value: string; label: string; count: number }> {
  const subscription = rows.filter(item => groupOf(item) === 'subscription')
  const named = subs
    .map(sub => ({
      value: sub.id,
      label: sub.name || sub.id,
      count: subscription.filter(item => !isRemovedSubscriptionItem(item) && item.providerId === sub.id).length,
    }))
    .sort((a, b) => b.count - a.count || a.label.localeCompare(b.label))
  const orphanCount = subscription.filter(isRemovedSubscriptionItem).length
  return [
    { value: 'all', label: t('全部提供方'), count: rows.length },
    { value: 'manual', label: t('手动'), count: rows.filter(item => groupOf(item) === 'manual').length },
    { value: 'clash', label: 'Clash Verge', count: rows.filter(item => groupOf(item) === 'clash').length },
    { value: 'subscription', label: t('全部订阅'), count: subscription.length },
    ...named,
    ...(orphanCount ? [{ value: ORPHAN_VENDOR, label: t('已移除的订阅'), count: orphanCount }] : []),
  ]
}

/** 搜索框左侧的放大镜（内联 SVG，照 models-page 的做法 —— 岛内不引图标库） */
function SearchIcon() {
  return <svg width='14' height='14' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' aria-hidden='true'><circle cx='11' cy='11' r='8' /><path d='m21 21-4.3-4.3' /></svg>
}

declare global {
  interface Window {
    wbProxiesPanel?: {
      load: () => Promise<void>
      activate: () => void
      visibleColumns: () => Array<{ key: string }>
    }
  }
}

window.wbProxiesPanel = {
  load: async () => { await loadPanel(); await loadSubscriptions() },
  activate: enteredPage,
  visibleColumns: () => visibleColumns().map(column => ({ key: column.key })),
}
const PAGE_SELECTOR = '.page[data-page="proxies"]'
function mount(): void {
  const section = document.querySelector<HTMLElement>(PAGE_SELECTOR)
  if (!section || section.dataset.mounted === '1') return
  section.dataset.mounted = '1'
  section.replaceChildren()
  createRoot(section).render(<ProxiesPage />)
  if (wb().wbApp?.currentPage === 'proxies') { enteredPage(); void loadPanel() }
}
if (document.querySelector(PAGE_SELECTOR)) mount()
else document.addEventListener('DOMContentLoaded', mount, { once: true })

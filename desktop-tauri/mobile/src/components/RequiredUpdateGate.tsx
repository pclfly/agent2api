import { useEffect, useState } from 'react'
import { Alert, Button, Dialog, DialogContent, DialogHeader, DialogTitle, DialogBody, DialogFooter } from '@ui'
import { apiCall } from '../api/client'
import type { ForceUpdatePolicy } from '../../../ui-shared/updates'
import { updateDownloadEnabled } from '../../../ui-shared/updates'
import { store, useStore } from '../state/store'
import { APP_VERSION } from '../lib/appUpdate'
import { t } from '../i18n'

export function RequiredUpdateGate() {
  const state = useStore()
  const [policy, setPolicy] = useState<ForceUpdatePolicy | null>(null)
  const [current, setCurrent] = useState('')
  const [updating, setUpdating] = useState(false)
  useEffect(() => {
    let disposed = false
    setPolicy(null)
    setUpdating(false)
    if (state.packaged && !state.server) return
    const load = () => { void apiCall<{ forceUpdate: ForceUpdatePolicy | null; currentVersion: string }>(`/api/update/policy${state.packaged ? `?current=${encodeURIComponent(APP_VERSION)}` : ''}`)
      .then(result => { if (!disposed) { setPolicy(result.forceUpdate); setCurrent(result.currentVersion) } }).catch(() => {}) }
    load()
    const timer = setInterval(load, 60_000)
    return () => { disposed = true; clearInterval(timer) }
  }, [state.serverRevision, state.packaged, state.server])
  // 下载入口被本地分发策略关掉时不拦人：这道门的唯一出口是「立即更新」→ 设置页，
  // 而那一页的下载按钮已经收掉（见 AppUpdatePanel）。留着它等于用一层无关闭按钮的
  // 全屏遮挡把人锁在门外，比「知道有新版本、自己决定何时升」更糟。策略是不替用户
  // 起下载，不是把人挡在门外 —— 与桌面端 update-required.tsx 同一口径。
  if (!updateDownloadEnabled()) return null
  if (!policy?.required || (updating && state.route.screen === 'settings')) return null
  return <Dialog open onOpenChange={() => {}}>
    <DialogContent showCloseButton={false}>
      <DialogHeader><DialogTitle>{t('需要强制更新')}</DialogTitle></DialogHeader>
      <DialogBody><Alert variant='warning'>{policy.message}</Alert>
        <p>{t('当前版本 {current}，最低可用版本 {minimum}。', { current, minimum: policy.minimumVersion })}</p>
        <p className='text-xs text-muted-foreground'>{t('更新前无法继续调用模型；账号、配置和历史记录不会被删除。')}</p>
      </DialogBody>
      <DialogFooter><Button onClick={() => { store.go('settings'); setUpdating(true) }}>{t('立即更新')}</Button></DialogFooter>
    </DialogContent>
  </Dialog>
}

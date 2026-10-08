import * as React from 'react'
import { createRoot } from 'react-dom/client'
import {
  Button,
  Dialog,
  DialogBody,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogSection,
  DialogTitle,
  RadioGroup,
  RadioGroupItem,
} from '@ui'
import type { AccountRecord } from './accounts-shared'

/**
 * 账号「出站身份」编辑弹窗（WB / VSC / CLI）。
 *
 * 参考 workbuddy2api-hub 的三身份设计：上游按客户端身份**分别计额**
 * （桌面端 / VSCode 插件 / CLI 各一条配额线），同一个账号在一条线上被限流时，
 * 换另一条线可能直接成功。身份是账号属性（后端 `product` 字段，缺省 `wb`）。
 *
 * 命令式接口：`window.wbAccountIdentityDialog.open(account)` / `.close()` ——
 * 与 conc-dialog.tsx 同一手法：点击时动态建、关闭即移除，不留常驻节点。
 * 保存走 `window.workbuddyDesktop.updateAccount`（PATCH `/api/accounts/{id}`，
 * 只带 `product` 一个字段；后端 apply_patch 是 patch 语义，没传的字段一概不动）。
 */

const OPTIONS = [
  {
    value: 'wb',
    label: 'WorkBuddy 桌面端（默认）',
    desc: '独立桌面客户端指纹（X-IDE-Type: WorkBuddy）',
  },
  {
    value: 'vsc',
    label: 'VSCode 插件',
    desc: '官方插件指纹（X-IDE-Type: VSCode；国内版端点走 www.workbuddy.cn）',
  },
  {
    value: 'cli',
    label: 'CodeBuddy CLI',
    desc: '官方 CLI 指纹（X-IDE-Type: CLI）',
  },
] as const

type Identity = (typeof OPTIONS)[number]['value']

/** 与后端 `identity::resolve_product` 同一口径（宽松解析，未知值回落 wb） */
function normalizeProduct(value: unknown): Identity {
  const text = String(value ?? '').trim().toLowerCase()
  if (text === 'vsc' || text === 'vscode' || text === 'ide') return 'vsc'
  if (text === 'cli' || text === 'codebuddy') return 'cli'
  return 'wb'
}

type IdentityDialogProps = {
  account: AccountRecord
  onClose: () => void
}

function IdentityDialog({ account, onClose }: IdentityDialogProps) {
  const [value, setValue] = React.useState<Identity>(normalizeProduct(account.product))
  const [saving, setSaving] = React.useState(false)
  const [hint, setHint] = React.useState('')
  const name = account.nickname || account.name || account.id

  async function handleSave() {
    setSaving(true)
    setHint('')
    try {
      await window.workbuddyDesktop.updateAccount(account.id, { product: value })
      onClose()
      window.wbApp?.toast?.('✅ 出站身份已保存')
      void window.wbApp?.refresh?.()
    } catch (error) {
      setHint(error instanceof Error ? error.message : String(error))
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => { if (!open) onClose() }}>
      <DialogContent className='w-[min(460px,calc(100vw-48px))]'>
        <DialogHeader>
          <DialogTitle>出站身份</DialogTitle>
        </DialogHeader>
        <DialogBody className='gap-3.5'>
          <DialogSection>
            <p className='text-[12.5px] leading-[1.6] text-subtle'>
              账号「{name}」以哪种客户端身份出站。上游按身份分别计额（桌面端 / VSCode
              插件 / CLI 各一条配额线）——一条线被限流时，换另一条线可能直接成功。
              改动立即生效，无需重启。
            </p>
          </DialogSection>
          <DialogSection>
            <RadioGroup
              value={value}
              onValueChange={(next) => setValue(next as Identity)}
              aria-label='出站身份'
            >
              {OPTIONS.map((option) => (
                <label
                  key={option.value}
                  className='flex cursor-pointer items-start gap-2.5 rounded-md px-1 py-1.5 hover:bg-surface-2'
                >
                  <RadioGroupItem value={option.value} className='mt-0.5' />
                  <span className='flex min-w-0 flex-col gap-0.5'>
                    <span className='text-[12.5px] text-foreground'>{option.label}</span>
                    <span className='text-[11px] leading-[1.5] text-muted-foreground'>
                      {option.desc}
                    </span>
                  </span>
                </label>
              ))}
            </RadioGroup>
          </DialogSection>
        </DialogBody>
        <DialogFooter>
          <span className='text-[11.5px] text-muted-foreground'>{hint}</span>
          <div className='mr-auto' />
          <Button variant='outline' onClick={onClose}>
            取消
          </Button>
          <Button variant='default' onClick={handleSave} disabled={saving}>
            {saving ? '保存中…' : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/* ─── 命令式外壳：与 conc-dialog.tsx 同一手法 ─── */

let root: ReturnType<typeof createRoot> | null = null
let host: HTMLElement | null = null

function unmountDialog() {
  if (root) {
    root.unmount()
    root = null
  }
  if (host) {
    host.remove()
    host = null
  }
}

function openDialog(account: AccountRecord) {
  if (!account?.id) return
  // 重复打开时先拆掉上一份（与 conc-dialog 同一处置）
  unmountDialog()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
  root.render(<IdentityDialog account={account} onClose={unmountDialog} />)
}

declare global {
  interface Window {
    // 与 conc-dialog.tsx 的同名声明保持**逐字一致**（接口合并要求同属性同类型，
    // 不一致会报 TS2717）
    workbuddyDesktop: {
      updateAccount(id: string, patch: Record<string, unknown>): Promise<unknown>
    }
    wbApp?: {
      toast?: (message: string, kind?: 'err' | 'ok') => void
      refresh?: () => Promise<void> | void
    }
    wbAccountIdentityDialog?: {
      open(account: AccountRecord): void
      close(): void
    }
  }
}

window.wbAccountIdentityDialog = { open: openDialog, close: unmountDialog }

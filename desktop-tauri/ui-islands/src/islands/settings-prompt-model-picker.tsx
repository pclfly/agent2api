import * as React from 'react'
import { Button, Checkbox, Dialog, DialogBody, DialogContent, DialogFooter, DialogHeader, DialogTitle, Input } from '@ui'
import { t } from '../i18n'
import { isPromptModelSelected, promptModelOptions, togglePromptModel, type PromptModelScope } from './settings-prompt-models'
import { saveProviderPromptModels, type ProviderPromptState } from './settings-state'

/** 草稿只在保存时提交，搜索不改变选择；目录不再包含的已选ID仍可见。 */
function ModelPicker({ item, label, catalog, disabled, onClose }: {
  item: ProviderPromptState; label: string; catalog: string[]; disabled: boolean; onClose: () => void
}) {
  const [draft, setDraft] = React.useState<PromptModelScope>(item.models === null ? null : [...item.models])
  const [query, setQuery] = React.useState('')
  const [saving, setSaving] = React.useState(false)
  const busy = disabled || saving
  const ids = promptModelOptions(catalog, item.models)
  const filtered = ids.filter(id => id.toLowerCase().includes(query.trim().toLowerCase()))
  const count = ids.filter(id => isPromptModelSelected(draft, id)).length
  async function commit() {
    setSaving(true)
    try { if (await saveProviderPromptModels(item, draft)) onClose() }
    finally { setSaving(false) }
  }
  return (
    <Dialog open onOpenChange={open => { if (!open && !busy) onClose() }}>
      <DialogContent className='w-[min(760px,calc(100vw-48px))]'>
        <DialogHeader><DialogTitle>{t('适用模型')} · {label}</DialogTitle></DialogHeader>
        <DialogBody>
          <p className='text-sm text-subtle'>
            {t('选中的模型使用该提供商的提示词模式；未选中的模型不应用网关提示词覆盖。模型别名按实际指向的上游模型判断。')}
          </p>
          <p className='text-sm text-subtle'>
            {t('替换模式会替换客户端 system/developer；追加模式保留并追加；透传模式不替换。此处不改变模型启用状态或网关自带提示词。')}
          </p>
          <div className='flex flex-wrap items-center gap-3'>
            <label className='inline-flex items-center gap-2 text-sm'>
              <Checkbox checked={draft === null} disabled={busy}
                aria-label={t('全部模型（含新增）')}
                onCheckedChange={checked => setDraft(checked === true ? null : [...ids])} />
              {t('全部模型（含新增）')}
            </label>
            <Button variant='outline' disabled={busy} onClick={() => setDraft([])}>{t('全不选')}</Button>
            <span className='text-sm text-subtle'>{draft === null ? t('默认全部勾选') : `${count} / ${ids.length}`}</span>
          </div>
          <Input aria-label={t('搜索模型')} placeholder={t('搜索模型')} value={query}
            onChange={event => setQuery(event.currentTarget.value)} disabled={busy} />
          <div className='max-h-[360px] overflow-y-auto rounded-md border border-line p-3'>
            {filtered.map(id => (
              <label key={id} className='flex cursor-pointer items-center gap-2 py-1.5 text-sm'>
                <Checkbox checked={isPromptModelSelected(draft, id)} disabled={busy} aria-label={id}
                  onCheckedChange={checked => setDraft(current => togglePromptModel(current, ids, id, checked === true))} />
                <span className='min-w-0 break-all'>{id}</span>
              </label>
            ))}
            {!filtered.length ? <p className='text-sm text-subtle'>{ids.length ? t('无匹配模型') : t('暂无本地模型清单；默认全部仍覆盖后续新增模型。')}</p> : null}
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

export function ProviderPromptModelsButton({ item, label, catalog, locked, busy }: {
  item: ProviderPromptState; label: string; catalog: string[]; locked: boolean; busy: boolean
}) {
  const [open, setOpen] = React.useState(false)
  const summary = item.models === null ? t('全部模型') : item.models.length ? `${item.models.length} ${t('个模型')}` : t('全不选')
  return <>
    <Button variant='outline' disabled={locked || busy} onClick={() => setOpen(true)}
      aria-label={`${label} ${t('适用模型')}`}>{t('适用模型')} · {summary}</Button>
    {open ? <ModelPicker item={item} label={label} catalog={catalog} disabled={busy} onClose={() => setOpen(false)} /> : null}
  </>
}

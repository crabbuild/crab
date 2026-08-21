"use client"

import { useState, useMemo } from "react"
import { pricingData } from "@/lib/pricing-data"
import {
  calculateMonthlyCost,
  validateCalculatorInput,
  CALCULATOR_RANGES,
  type CalculatorInputs,
  type ValidationResult,
} from "@/lib/pricing-calculator"
import { cn } from "@/lib/utils"

type NumericField = "repositorySizeGb" | "monthlyPushes" | "monthlyPulls"

interface FieldErrors {
  repositorySizeGb?: string
  monthlyPushes?: string
  monthlyPulls?: string
  provider?: string
  region?: string
  storageClass?: string
}

export function CostCalculator() {
  const [repositorySizeGb, setRepositorySizeGb] = useState(100)
  const [monthlyPushes, setMonthlyPushes] = useState(30)
  const [monthlyPulls, setMonthlyPulls] = useState(100)
  const [provider, setProvider] = useState("aws-s3")
  const [region, setRegion] = useState("us-east-1")
  const [storageClass, setStorageClass] = useState("standard")
  const [errors, setErrors] = useState<FieldErrors>({})

  // Cascading dropdown data
  const selectedProvider = useMemo(
    () => pricingData.providers.find((p) => p.id === provider),
    [provider],
  )

  const availableRegions = useMemo(
    () => selectedProvider?.regions ?? [],
    [selectedProvider],
  )

  const selectedRegion = useMemo(
    () => availableRegions.find((r) => r.id === region),
    [availableRegions, region],
  )

  const availableStorageClasses = useMemo(
    () => selectedRegion?.storageClasses ?? [],
    [selectedRegion],
  )

  // Validate a field and update errors state
  function validateField(
    field: keyof CalculatorInputs,
    value: number | string,
  ): boolean {
    const result: ValidationResult = validateCalculatorInput(field, value)
    setErrors((prev) => {
      if (result.valid) {
        const next = { ...prev }
        delete next[field]
        return next
      }
      return { ...prev, [field]: result.error }
    })
    return result.valid
  }

  // Handle numeric input changes
  function handleNumericChange(field: NumericField, raw: string) {
    const numeric = Number(raw)
    validateField(field, raw)

    switch (field) {
      case "repositorySizeGb":
        setRepositorySizeGb(Number.isNaN(numeric) ? 0 : numeric)
        break
      case "monthlyPushes":
        setMonthlyPushes(Number.isNaN(numeric) ? 0 : numeric)
        break
      case "monthlyPulls":
        setMonthlyPulls(Number.isNaN(numeric) ? 0 : numeric)
        break
    }
  }

  // Handle provider change — cascade region and storage class
  function handleProviderChange(newProvider: string) {
    setProvider(newProvider)
    validateField("provider", newProvider)

    const providerData = pricingData.providers.find(
      (p) => p.id === newProvider,
    )
    if (providerData && providerData.regions.length > 0) {
      const firstRegion = providerData.regions[0]
      setRegion(firstRegion.id)

      const recommended = firstRegion.storageClasses.find(
        (c) => c.recommended,
      )
      setStorageClass(recommended?.id ?? firstRegion.storageClasses[0]?.id ?? "")
    }
  }

  // Handle region change — cascade storage class
  function handleRegionChange(newRegion: string) {
    setRegion(newRegion)
    validateField("region", newRegion)

    const regionData = selectedProvider?.regions.find(
      (r) => r.id === newRegion,
    )
    if (regionData && regionData.storageClasses.length > 0) {
      const recommended = regionData.storageClasses.find(
        (c) => c.recommended,
      )
      setStorageClass(
        recommended?.id ?? regionData.storageClasses[0]?.id ?? "",
      )
    }
  }

  function handleStorageClassChange(newClass: string) {
    setStorageClass(newClass)
    validateField("storageClass", newClass)
  }

  // Compute cost only when all inputs are valid
  const allValid = Object.keys(errors).length === 0
  const cost = useMemo(() => {
    if (!allValid) return null

    const rates = availableStorageClasses.find((c) => c.id === storageClass)
    if (!rates) return null

    const inputs: CalculatorInputs = {
      repositorySizeGb,
      monthlyPushes,
      monthlyPulls,
      provider,
      region,
      storageClass,
    }

    return calculateMonthlyCost(inputs, rates)
  }, [
    allValid,
    repositorySizeGb,
    monthlyPushes,
    monthlyPulls,
    provider,
    region,
    storageClass,
    availableStorageClasses,
  ])

  return (
    <div className="rounded-(--card-radius) border border-border bg-card p-(--card-padding) shadow-card">
      <h3 className="mb-6 text-heading-sm font-semibold text-card-foreground">
        Estimate Your Monthly Cost
      </h3>

      <div className="grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
        {/* Repository Size */}
        <NumericInput
          id="calc-repo-size"
          label="Repository Size (GB)"
          value={repositorySizeGb}
          min={CALCULATOR_RANGES.repositorySizeGb.min}
          max={CALCULATOR_RANGES.repositorySizeGb.max}
          step={0.1}
          error={errors.repositorySizeGb}
          onChange={(v) => handleNumericChange("repositorySizeGb", v)}
        />

        {/* Monthly Pushes */}
        <NumericInput
          id="calc-monthly-pushes"
          label="Monthly Pushes"
          value={monthlyPushes}
          min={CALCULATOR_RANGES.monthlyPushes.min}
          max={CALCULATOR_RANGES.monthlyPushes.max}
          step={1}
          error={errors.monthlyPushes}
          onChange={(v) => handleNumericChange("monthlyPushes", v)}
        />

        {/* Monthly Pulls */}
        <NumericInput
          id="calc-monthly-pulls"
          label="Monthly Pulls"
          value={monthlyPulls}
          min={CALCULATOR_RANGES.monthlyPulls.min}
          max={CALCULATOR_RANGES.monthlyPulls.max}
          step={1}
          error={errors.monthlyPulls}
          onChange={(v) => handleNumericChange("monthlyPulls", v)}
        />

        {/* Provider */}
        <SelectInput
          id="calc-provider"
          label="Cloud Provider"
          value={provider}
          error={errors.provider}
          onChange={handleProviderChange}
          options={pricingData.providers.map((p) => ({
            value: p.id,
            label: p.name,
          }))}
        />

        {/* Region */}
        <SelectInput
          id="calc-region"
          label="Region"
          value={region}
          error={errors.region}
          onChange={handleRegionChange}
          options={availableRegions.map((r) => ({
            value: r.id,
            label: r.name,
          }))}
        />

        {/* Storage Class */}
        <SelectInput
          id="calc-storage-class"
          label="Storage Class"
          value={storageClass}
          error={errors.storageClass}
          onChange={handleStorageClassChange}
          options={availableStorageClasses.map((c) => ({
            value: c.id,
            label: c.name,
          }))}
        />
      </div>

      {/* Cost Display */}
      <div className="mt-6 rounded-(--card-radius) border border-primary/20 bg-primary-muted p-5 text-center">
        {cost !== null ? (
          <>
            <p className="text-sm font-medium text-muted-foreground">
              Estimated Monthly Cost
            </p>
            <p className="mt-1 text-heading-xl font-bold text-primary">
              ${cost.toFixed(2)}
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              Based on {selectedProvider?.name ?? provider} /{" "}
              {selectedRegion?.name ?? region} /{" "}
              {availableStorageClasses.find((c) => c.id === storageClass)
                ?.name ?? storageClass}
            </p>
          </>
        ) : (
          <p className="text-sm text-muted-foreground">
            Fix validation errors above to see your estimate.
          </p>
        )}
      </div>
    </div>
  )
}

// --- Sub-components ---

interface NumericInputProps {
  id: string
  label: string
  value: number
  min: number
  max: number
  step: number
  error?: string
  onChange: (raw: string) => void
}

function NumericInput({
  id,
  label,
  value,
  min,
  max,
  step,
  error,
  onChange,
}: NumericInputProps) {
  return (
    <div className="flex flex-col gap-1.5">
      <label
        htmlFor={id}
        className="text-sm font-medium text-foreground"
      >
        {label}
      </label>
      <input
        id={id}
        type="number"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={cn(
          "h-10 w-full rounded-md border bg-background px-3 text-sm text-foreground",
          "transition-colors duration-(--duration-fast)",
          "focus:outline-none focus:ring-2 focus:ring-ring",
          error ? "border-destructive" : "border-input",
        )}
      />
      {error && (
        <p className="text-xs text-destructive" role="alert">
          {error}
        </p>
      )}
    </div>
  )
}

interface SelectInputProps {
  id: string
  label: string
  value: string
  error?: string
  onChange: (value: string) => void
  options: Array<{ value: string; label: string }>
}

function SelectInput({
  id,
  label,
  value,
  error,
  onChange,
  options,
}: SelectInputProps) {
  return (
    <div className="flex flex-col gap-1.5">
      <label
        htmlFor={id}
        className="text-sm font-medium text-foreground"
      >
        {label}
      </label>
      <select
        id={id}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={cn(
          "h-10 w-full rounded-md border bg-background px-3 text-sm text-foreground",
          "transition-colors duration-(--duration-fast)",
          "focus:outline-none focus:ring-2 focus:ring-ring",
          error ? "border-destructive" : "border-input",
        )}
      >
        {options.map((opt) => (
          <option key={opt.value} value={opt.value}>
            {opt.label}
          </option>
        ))}
      </select>
      {error && (
        <p className="text-xs text-destructive" role="alert">
          {error}
        </p>
      )}
    </div>
  )
}

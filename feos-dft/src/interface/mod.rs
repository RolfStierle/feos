//! Density profiles at planar interfaces and interfacial tensions.
use crate::convolver::ConvolverFFT;
use crate::functional::{HelmholtzEnergyFunctional, DFT};
use crate::geometry::{Axis, Grid};
use crate::profile::{DFTProfile, DFTSpecifications};
use crate::solver::DFTSolver;
use feos_core::{Contributions, EosError, EosResult, EosUnit, PhaseEquilibrium};
use ndarray::{s, Array, Array1, Array2, Axis as Axis_nd, Ix1};
use quantity::{QuantityArray1, QuantityArray2, QuantityScalar};

mod surface_tension_diagram;
pub use surface_tension_diagram::SurfaceTensionDiagram;

const RELATIVE_WIDTH: f64 = 6.0;
const MIN_WIDTH: f64 = 100.0;

/// Density profile and properties of a planar interface.
pub struct PlanarInterface<U: EosUnit, F: HelmholtzEnergyFunctional> {
    pub profile: DFTProfile<U, Ix1, F>,
    pub vle: PhaseEquilibrium<U, DFT<F>, 2>,
    pub surface_tension: Option<QuantityScalar<U>>,
    pub equimolar_radius: Option<QuantityScalar<U>>,
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> Clone for PlanarInterface<U, F> {
    fn clone(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            vle: self.vle.clone(),
            surface_tension: self.surface_tension,
            equimolar_radius: self.equimolar_radius,
        }
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> PlanarInterface<U, F> {
    pub fn solve_inplace(&mut self, solver: Option<&DFTSolver>, debug: bool) -> EosResult<()> {
        // Solve the profile
        self.profile.solve(solver, debug)?;

        // postprocess
        self.surface_tension = Some(self.profile.integrate(
            &(self.profile.grand_potential_density()?
                + self.vle.vapor().pressure(Contributions::Total)),
        ));
        let delta_rho = self.vle.liquid().density - self.vle.vapor().density;
        self.equimolar_radius = Some(
            self.profile
                .integrate(&(self.profile.density.sum_axis(Axis_nd(0)) - self.vle.vapor().density))
                / delta_rho,
        );

        Ok(())
    }

    pub fn solve(mut self, solver: Option<&DFTSolver>) -> EosResult<Self> {
        self.solve_inplace(solver, false)?;
        Ok(self)
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> PlanarInterface<U, F> {
    pub fn new(
        vle: &PhaseEquilibrium<U, DFT<F>, 2>,
        n_grid: usize,
        l_grid: QuantityScalar<U>,
    ) -> EosResult<Self> {
        let dft = &vle.vapor().eos;

        // generate grid
        let grid = Grid::Cartesian1(Axis::new_cartesian(n_grid, l_grid, None)?);

        // initialize convolver
        let t = vle
            .vapor()
            .temperature
            .to_reduced(U::reference_temperature())?;
        let weight_functions = dft.weight_functions(t);
        let convolver = ConvolverFFT::plan(&grid, &weight_functions, None);

        Ok(Self {
            profile: DFTProfile::new(grid, convolver, vle.vapor(), None, None)?,
            vle: vle.clone(),
            surface_tension: None,
            equimolar_radius: None,
        })
    }

    pub fn from_tanh(
        vle: &PhaseEquilibrium<U, DFT<F>, 2>,
        n_grid: usize,
        l_grid: QuantityScalar<U>,
        critical_temperature: QuantityScalar<U>,
    ) -> EosResult<Self> {
        let mut profile = Self::new(vle, n_grid, l_grid)?;

        // calculate segment indices
        let indices = &profile.profile.dft.component_index();

        // calculate density profile
        let z0 = 0.5 * l_grid.to_reduced(U::reference_length())?;
        let (z0, sign) = (z0.abs(), -z0.signum());
        let reduced_temperature = vle.vapor().temperature.to_reduced(critical_temperature)?;
        profile.profile.density =
            QuantityArray2::from_shape_fn(profile.profile.density.raw_dim(), |(i, z)| {
                let rho_v = profile.vle.vapor().partial_density.get(indices[i]);
                let rho_l = profile.vle.liquid().partial_density.get(indices[i]);
                0.5 * (rho_l - rho_v)
                    * (sign * (profile.profile.grid.grids()[0][z] - z0) / 3.0
                        * (2.4728 - 2.3625 * reduced_temperature))
                        .tanh()
                    + 0.5 * (rho_l + rho_v)
            });

        // specify specification
        profile.profile.specification =
            DFTSpecifications::total_moles_from_profile(&profile.profile)?;

        Ok(profile)
    }

    pub fn from_pdgt(vle: &PhaseEquilibrium<U, DFT<F>, 2>, n_grid: usize) -> EosResult<Self> {
        let dft = &vle.vapor().eos;

        if dft.component_index().len() != 1 {
            panic!("Initialization from pDGT not possible for segment DFT or mixtures");
        }

        // calculate density profile from pDGT
        let n_grid_pdgt = 20;
        let mut z_pdgt = Array1::zeros(n_grid_pdgt) * U::reference_length();
        let mut w_pdgt = U::reference_length();
        let (rho_pdgt, gamma_pdgt) =
            dft.solve_pdgt(vle, 20, 0, Some((&mut z_pdgt, &mut w_pdgt)))?;
        if !gamma_pdgt
            .to_reduced(U::reference_surface_tension())?
            .is_normal()
        {
            return Err(EosError::InvalidState(
                String::from("DFTProfile::from_pdgt"),
                String::from("gamma_pdgt"),
                gamma_pdgt.to_reduced(U::reference_surface_tension())?,
            ));
        }

        // create PlanarInterface
        let l_grid = (MIN_WIDTH * U::reference_length())
            .max(w_pdgt * RELATIVE_WIDTH)
            .unwrap();
        let mut profile = Self::new(vle, n_grid, l_grid)?;

        // interpolate density profile from pDGT to DFT
        let r = l_grid * 0.5;
        profile.profile.density = interp_symmetric(
            vle,
            z_pdgt,
            rho_pdgt,
            &profile.vle,
            profile.profile.grid.grids()[0],
            r,
        )?;

        // specify specification
        profile.profile.specification =
            DFTSpecifications::total_moles_from_profile(&profile.profile)?;

        Ok(profile)
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> PlanarInterface<U, F> {
    pub fn shift_equimolar_inplace(&mut self) {
        let s = self.profile.density.shape();
        let m = &self.profile.dft.m();
        let mut rho_l = 0.0 * U::reference_density();
        let mut rho_v = 0.0 * U::reference_density();
        let mut rho = Array::zeros(s[1]) * U::reference_density();
        for i in 0..s[0] {
            rho_l += self.profile.density.get((i, 0)) * m[i];
            rho_v += self.profile.density.get((i, s[1] - 1)) * m[i];
            rho += &(&self.profile.density.index_axis(Axis_nd(0), i) * m[i]);
        }

        let x = (rho - rho_v) / (rho_l - rho_v);
        let ze = self.profile.grid.axes()[0].edges[0]
            + self
                .profile
                .integrate(&x)
                .to_reduced(U::reference_length())
                .unwrap();
        self.profile.grid.axes_mut()[0].grid -= ze;
    }

    pub fn shift_equimolar(mut self) -> Self {
        self.shift_equimolar_inplace();
        self
    }

    /// Relative adsorption of component `i' with respect to `j': \Gamma_i^(j)
    pub fn relative_adsorption(&self) -> EosResult<QuantityArray2<U>> {
        let s = self.profile.density.shape();
        let mut rho_l = Array1::zeros(s[0]) * U::reference_density();
        let mut rho_v = Array1::zeros(s[0]) * U::reference_density();

        // Calculate the partial densities in the liquid and in the vapor phase
        for i in 0..s[0] {
            // rho_l.try_set(i, self.profile.density.get((i, 0)) * m[i])?;
            // rho_v.try_set(i, self.profile.density.get((i, s[1] - 1)) * m[i])?;
            rho_l.try_set(i, self.profile.density.get((i, 0)))?;
            rho_v.try_set(i, self.profile.density.get((i, s[1] - 1)))?;
        }

        // Calculate \Gamma_i^(j)
        let gamma = Array2::from_shape_fn((s[0], s[0]), |(i, j)| -> f64 {
            if i == j {
                0.0
            } else {
                (-(rho_l.get(i) - rho_v.get(i))
                    * ((&self.profile.density.index_axis(Axis_nd(0), j) - rho_l.get(j))
                        / (rho_l.get(j) - rho_v.get(j))
                        - (&self.profile.density.index_axis(Axis_nd(0), i) - rho_l.get(i))
                            / (rho_l.get(i) - rho_v.get(i)))
                    .integrate(&self.profile.grid.integration_weights_unit()))
                .to_reduced(U::reference_density() * U::reference_length())
                .unwrap()
            }
        });

        Ok(gamma * U::reference_density() * U::reference_length())
    }

    /// Interfacial enrichment of component `i': E_i
    pub fn interfacial_enrichment(&self) -> EosResult<Array1<f64>> {
        let s = self.profile.density.shape();
        let mut rho_l = Array1::zeros(s[0]) * U::reference_density();
        let mut rho_v = Array1::zeros(s[0]) * U::reference_density();

        // Calculate the partial densities in the liquid and in the vapor phase
        for i in 0..s[0] {
            rho_l.try_set(i, self.profile.density.get((i, 0)))?;
            rho_v.try_set(i, self.profile.density.get((i, s[1] - 1)))?;
        }

        // Calculate interfacial enrichment E_i
        let enrichment = Array1::from_shape_fn(s[0], |i| {
            (*(self
                .profile
                .density
                .index_axis(Axis_nd(0), i)
                .to_owned()
                .to_reduced(U::reference_density())
                .unwrap()
                .iter()
                .max_by(|&a, &b| a.total_cmp(b))
                .unwrap())
                * U::reference_density()
                / (rho_l.get(i).max(rho_v.get(i)).unwrap()))
            .into_value()
            .unwrap()
        });

        Ok(enrichment)
    }

    /// Interface thickness (90-10 number density difference)
    pub fn interfacial_thickness(&self, limits: (f64, f64)) -> EosResult<QuantityScalar<U>> {
        let s = self.profile.density.shape();
        let rho = self
            .profile
            .density
            .sum_axis(Axis_nd(0))
            .to_reduced(U::reference_density())?;
        let z = self.profile.grid.grids()[0];
        let dz = z[1] - z[0];
        let (limit_upper, limit_lower) = if limits.0 > limits.1 {
            (limits.0, limits.1)
        } else {
            (limits.1, limits.0)
        };

        if limit_upper >= 1.0 || limit_upper.is_sign_negative() {
            panic!("Upper limit 'l' of interface thickness needs to satisfy 0 < l < 1.");
        }
        if limit_lower >= 1.0 || limit_lower.is_sign_negative() {
            panic!("Lower limit 'l' of interface thickness needs to satisfy 0 < l < 1.");
        }

        // Get the densities in the liquid and in the vapor phase
        let rho_l = if rho.get(0) > rho.get(s[1] - 1) {
            rho[0]
        } else {
            rho[s[1] - 1]
        };
        let rho_v = if rho.get(0) > rho.get(s[1] - 1) {
            rho[s[1] - 1]
        } else {
            rho[0]
        };

        // Density boundaries for interface definition
        let rho_upper = rho_v + limit_upper * (rho_l - rho_v);
        let rho_lower = rho_v + limit_lower * (rho_l - rho_v);

        // Get indizes right of intersection between density profile and
        // constant density boundaries
        let index_upper_plus = rho
            .iter()
            .enumerate()
            .find(|(_, &x)| (x - rho_upper).is_sign_negative())
            .expect("Could not find rho_upper value!")
            .0;
        let index_lower_plus = rho
            .iter()
            .enumerate()
            .find(|(_, &x)| (x - rho_lower).is_sign_negative())
            .expect("Could not find rho_lower value!")
            .0;

        // Calculate distance between two density points using a linear
        // interpolated density profiles between the two grid points where the
        // density profile crosses the limiting densities
        let z_upper = z[index_upper_plus - 1]
            + (rho_upper - rho[index_upper_plus - 1])
                / (rho[index_upper_plus] - rho[index_upper_plus - 1])
                * dz;
        let z_lower = z[index_lower_plus - 1]
            + (rho_lower - rho[index_lower_plus - 1])
                / (rho[index_lower_plus] - rho[index_lower_plus - 1])
                * dz;

        // Return
        Ok((z_lower - z_upper) * U::reference_length())
    }

    fn set_density_scale(&mut self, init: &QuantityArray2<U>) {
        assert_eq!(self.profile.density.shape(), init.shape());
        let n_grid = self.profile.density.shape()[1];
        let drho_init = &init.index_axis(Axis_nd(1), 0) - &init.index_axis(Axis_nd(1), n_grid - 1);
        let rho_init_0 = init.index_axis(Axis_nd(1), n_grid - 1);
        let drho = &self.profile.density.index_axis(Axis_nd(1), 0)
            - &self.profile.density.index_axis(Axis_nd(1), n_grid - 1);
        let rho_0 = self.profile.density.index_axis(Axis_nd(1), n_grid - 1);

        self.profile.density =
            QuantityArray2::from_shape_fn(self.profile.density.raw_dim(), |(i, j)| {
                (init.get((i, j)) - rho_init_0.get(i))
                    .to_reduced(drho_init.get(i))
                    .unwrap()
                    * drho.get(i)
                    + rho_0.get(i)
            });
    }

    pub fn set_density_inplace(&mut self, init: &QuantityArray2<U>, scale: bool) {
        if scale {
            self.set_density_scale(init)
        } else {
            assert_eq!(self.profile.density.shape(), init.shape());
            self.profile.density = init.clone();
        }
    }

    pub fn set_density(mut self, init: &QuantityArray2<U>, scale: bool) -> Self {
        self.set_density_inplace(init, scale);
        self
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> PlanarInterface<U, F> {}

fn interp_symmetric<U: EosUnit, F: HelmholtzEnergyFunctional>(
    vle_pdgt: &PhaseEquilibrium<U, DFT<F>, 2>,
    z_pdgt: QuantityArray1<U>,
    rho_pdgt: QuantityArray2<U>,
    vle: &PhaseEquilibrium<U, DFT<F>, 2>,
    z: &Array1<f64>,
    radius: QuantityScalar<U>,
) -> EosResult<QuantityArray2<U>> {
    let reduced_density = Array2::from_shape_fn(rho_pdgt.raw_dim(), |(i, j)| {
        (rho_pdgt.get((i, j)) - vle_pdgt.vapor().partial_density.get(i))
            .to_reduced(
                vle_pdgt.liquid().partial_density.get(i) - vle_pdgt.vapor().partial_density.get(i),
            )
            .unwrap()
            - 0.5
    });
    let segments = vle_pdgt.vapor().eos.component_index().len();
    let mut reduced_density = interp(
        &z_pdgt.to_reduced(U::reference_length())?,
        &reduced_density,
        &(z - radius.to_reduced(U::reference_length())?),
        &Array1::from_elem(segments, 0.5),
        &Array1::from_elem(segments, -0.5),
        false,
    ) + interp(
        &z_pdgt.to_reduced(U::reference_length())?,
        &reduced_density,
        &(z + radius.to_reduced(U::reference_length())?),
        &Array1::from_elem(segments, -0.5),
        &Array1::from_elem(segments, 0.5),
        true,
    );
    if radius < 0.0 * U::reference_length() {
        reduced_density += 1.0;
    }
    Ok(QuantityArray2::from_shape_fn(
        reduced_density.raw_dim(),
        |(i, j)| {
            reduced_density[(i, j)]
                * (vle.liquid().partial_density.get(i) - vle.vapor().partial_density.get(i))
                + vle.vapor().partial_density.get(i)
        },
    ))
}

fn interp(
    x_old: &Array1<f64>,
    y_old: &Array2<f64>,
    x_new: &Array1<f64>,
    y_left: &Array1<f64>,
    y_right: &Array1<f64>,
    reverse: bool,
) -> Array2<f64> {
    let n = x_old.len();

    let (x_rev, y_rev) = if reverse {
        (-&x_old.slice(s![..;-1]), y_old.slice(s![.., ..;-1]))
    } else {
        (x_old.to_owned(), y_old.view())
    };

    let mut y_new = Array2::zeros((y_rev.shape()[0], x_new.len()));
    let mut k = 0;
    for i in 0..x_new.len() {
        while k < n && x_new[i] > x_rev[k] {
            k += 1;
        }
        y_new.slice_mut(s![.., i]).assign(&if k == 0 {
            y_left
                + &((&y_rev.slice(s![.., 0]) - y_left)
                    * ((&y_rev.slice(s![.., 1]) - y_left) / (&y_rev.slice(s![.., 0]) - y_left))
                        .mapv(|x| x.powf((x_new[i] - x_rev[0]) / (x_rev[1] - x_rev[0]))))
        } else if k == n {
            y_right
                + &((&y_rev.slice(s![.., n - 2]) - y_right)
                    * ((&y_rev.slice(s![.., n - 1]) - y_right)
                        / (&y_rev.slice(s![.., n - 2]) - y_right))
                        .mapv(|x| {
                            x.powf((x_new[i] - x_rev[n - 2]) / (x_rev[n - 1] - x_rev[n - 2]))
                        }))
        } else {
            &y_rev.slice(s![.., k - 1])
                + &((x_new[i] - x_rev[k - 1]) / (x_rev[k] - x_rev[k - 1])
                    * (&y_rev.slice(s![.., k]) - &y_rev.slice(s![.., k - 1])))
        });
    }
    y_new
}
